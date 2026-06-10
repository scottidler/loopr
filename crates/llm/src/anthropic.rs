//! `AnthropicClient`: the one concrete `LlmClient` impl shipped in
//! Stage 6. Talks to Anthropic's Messages API with `tool_choice`
//! locked to a caller-supplied tool. No SSE, no multi-turn, no retry.
//!
//! Seam layering matches the trait's contract: one buffered HTTP call
//! per invocation; the status code and response shape are classified
//! into `LlmError` variants per the table in the design doc; the
//! first matching `tool_use` content block becomes the returned
//! `ToolCall`.

use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, info_span, instrument, warn};

use crate::client::LlmClient;
use crate::config::LlmConfig;
use crate::error::{FatalReason, LlmError};
use crate::message::{Message, MessageContent};
use crate::tool::{ToolCall, ToolSchema};
use crate::usage::Usage;

/// Maximum bytes of `system` / `user` prompt to emit as a span field.
/// Beyond this, the span records a truncated preview plus the
/// original byte-length suffix so `events.log` stays bounded on long
/// prompts without losing debugging signal. Locked by design doc
/// Security → Telemetry payload rules.
const PROMPT_PREVIEW_MAX_BYTES: usize = 4096;

/// Wall-clock ceiling for a single Anthropic call. Sonnet with 8192
/// max_tokens usually completes in 1-10s; 120s is 12x headroom for
/// slow paths without letting a hung call stall the decomposer
/// indefinitely. The daemon is async, so other IPC traffic is
/// unaffected while this call is outstanding; the ceiling exists
/// only to guarantee the decomposer's request handler eventually
/// returns an error instead of hanging forever.
const REQUEST_TIMEOUT_SECS: u64 = 120;

/// Anthropic Messages API version header value. Pinned to a known-
/// stable date; upgrade deliberately when a specific newer feature is
/// needed. Do not track-the-latest.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Concrete `LlmClient` backed by the Anthropic Messages API.
///
/// Holds a `reqwest::Client` configured with the API key baked into
/// `default_headers` as a sensitive `HeaderValue`. The raw API key
/// string is dropped by the time the constructor returns, so no
/// struct field holds the secret; `reqwest`'s own `Debug` impl
/// redacts sensitive header values automatically.
pub struct AnthropicClient {
    http: reqwest::Client,
    config: LlmConfig,
}

impl AnthropicClient {
    /// Build a new client. `api_key` is consumed and dropped inside
    /// this function; only a sensitive `HeaderValue` clone survives
    /// in `http`'s default headers.
    ///
    /// Returns `Fatal(ConfigInvalid)` if the key is empty, the key
    /// is not a valid HTTP header value (e.g. embedded control
    /// characters), or the underlying `reqwest::Client::build` fails
    /// (rare; usually TLS backend misconfiguration).
    #[instrument(
        name = "llm.anthropic.new",
        level = "info",
        skip_all,
        fields(
            model = %config.model,
            api_base_url = %config.api_base_url,
            max_tokens = config.max_tokens,
        ),
        err,
    )]
    pub fn new(config: LlmConfig, api_key: String) -> Result<Self, LlmError> {
        if api_key.is_empty() {
            return Err(LlmError::Fatal {
                reason: FatalReason::ConfigInvalid("empty API key".into()),
            });
        }

        validate_api_base_url(&config.api_base_url)?;

        let mut headers = HeaderMap::new();
        let mut key_val = HeaderValue::from_str(&api_key).map_err(|e| LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(format!("invalid API key header: {e}")),
        })?;
        key_val.set_sensitive(true);
        headers.insert("x-api-key", key_val);
        headers.insert("anthropic-version", HeaderValue::from_static(ANTHROPIC_VERSION));

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| LlmError::Fatal {
                reason: FatalReason::ConfigInvalid(format!("reqwest client build failed: {e}")),
            })?;

        Ok(Self { http, config })
    }
}

/// Request-body shape for Anthropic's Messages API. Serialized via
/// `serde_json`; field order is the Messages-API documented order.
///
/// `system` is a `serde_json::Value` holding a one-element array of
/// content blocks (so we can attach `cache_control: ephemeral` to the
/// system block). The Messages API also accepts a bare string for
/// `system`; the array form is required when any block carries
/// `cache_control`. See `build_system_block`.
#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    system: Value,
    messages: [AnthropicMessage<'a>; 1],
    tools: [ToolSchemaWire<'a>; 1],
    tool_choice: ToolChoiceWire<'a>,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ToolSchemaWire<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

#[derive(Serialize)]
struct ToolChoiceWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
}

/// Request-body shape for `complete_free`. No `tools`, no
/// `tool_choice`: the model replies with plain content blocks.
/// Same `system: Value` shape as `AnthropicRequest` so the cached
/// system block carries across both call paths.
///
/// `content` on each message is a `Value` rather than `&str` because
/// multi-block turns (tool-use arcs) require the content-block array
/// form `[{"type":"...","text":"..."}]` while plain-text turns may use
/// a bare string. `to_wire_content` handles the conversion.
#[derive(Serialize)]
struct AnthropicFreeRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    system: Value,
    messages: Vec<AnthropicFreeMessage>,
}

#[derive(Serialize)]
struct AnthropicFreeMessage {
    role: String,
    content: Value,
}

/// Build the `system` field as a one-element content-block array with
/// `cache_control: { "type": "ephemeral" }`. Anthropic charges for the
/// first request that crosses the per-model cacheable threshold (1024
/// tokens for Haiku, 2048 for Sonnet/Opus 4.x) and reads from the
/// cache on subsequent matching requests, surfacing the result in
/// `usage.cache_creation_input_tokens` / `usage.cache_read_input_tokens`.
/// Below the threshold, the cache silently no-ops; the wire shape is
/// the same either way.
fn build_system_block(system: &str) -> Value {
    serde_json::json!([{
        "type": "text",
        "text": system,
        "cache_control": { "type": "ephemeral" }
    }])
}

impl LlmClient for AnthropicClient {
    async fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: ToolSchema,
        model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        // Span fields per design doc Security → Telemetry rules:
        // model + prompt byte-lengths + previews (truncated to
        // PROMPT_PREVIEW_MAX_BYTES) + duration. NEVER the tool
        // schema, ToolCall.input, headers, or API key.
        let effective_model = model.unwrap_or(&self.config.model);
        let span = info_span!(
            "llm.anthropic",
            model = %effective_model,
            system_len = system.len(),
            user_len = user.len(),
            system_preview = %truncate_preview(system),
            user_preview = %truncate_preview(user),
            tool_name = %tool.name,
            temperature_present = self.config.temperature.is_some(),
            duration_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            input_tokens = tracing::field::Empty,
            output_tokens = tracing::field::Empty,
            cost_micros = tracing::field::Empty,
            model_returned = tracing::field::Empty,
            cache_creation_input_tokens = tracing::field::Empty,
            cache_read_input_tokens = tracing::field::Empty,
            cache_hit_ratio = tracing::field::Empty,
        );
        let _enter = span.enter();
        let started = Instant::now();

        let result = self.send_request(system, user, &tool, effective_model).await;

        let elapsed = started.elapsed().as_millis() as u64;
        span.record("duration_ms", elapsed);
        // Record token + cost fields on BOTH success and billed-error
        // (max_tokens truncation / schema refusal): those 200s cost
        // tokens that must show on the cost span too (vision cost audit).
        if let Some(usage) = usage_to_record(&result) {
            record_usage_span(&span, effective_model, usage);
        }
        match &result {
            Ok((_, usage)) => {
                span.record("outcome", "ok");
                debug!(
                    target: "llm.anthropic.cache",
                    created = usage.cache_creation_input_tokens,
                    read = usage.cache_read_input_tokens,
                    ratio = usage.cache_hit_ratio(),
                    "llm.anthropic call succeeded"
                );
            }
            Err(LlmError::Retryable { reason }) => {
                span.record("outcome", "retryable");
                warn!(reason = %reason, "llm.anthropic call failed (retryable)");
            }
            Err(LlmError::Fatal { reason }) => {
                span.record("outcome", "fatal");
                warn!(reason = ?reason, "llm.anthropic call failed (fatal)");
            }
        }
        result
    }

    async fn complete_free(
        &self,
        system: &str,
        messages: &[Message],
        model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        let last_user_preview = messages
            .last()
            .and_then(|m| m.content.first())
            .and_then(|c| {
                if let MessageContent::Text { text } = c {
                    Some(truncate_preview(text))
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let effective_model = model.unwrap_or(&self.config.model);
        let span = info_span!(
            "llm.anthropic.free",
            model = %effective_model,
            system_len = system.len(),
            message_count = messages.len(),
            system_preview = %truncate_preview(system),
            last_user_preview = %last_user_preview,
            temperature_present = self.config.temperature.is_some(),
            duration_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            input_tokens = tracing::field::Empty,
            output_tokens = tracing::field::Empty,
            cost_micros = tracing::field::Empty,
            model_returned = tracing::field::Empty,
            cache_creation_input_tokens = tracing::field::Empty,
            cache_read_input_tokens = tracing::field::Empty,
            cache_hit_ratio = tracing::field::Empty,
        );
        let _enter = span.enter();
        let started = Instant::now();

        let result = self.send_free_request(system, messages, effective_model).await;

        let elapsed = started.elapsed().as_millis() as u64;
        span.record("duration_ms", elapsed);
        if let Some(usage) = usage_to_record(&result) {
            record_usage_span(&span, effective_model, usage);
        }
        match &result {
            Ok((_, usage)) => {
                span.record("outcome", "ok");
                debug!(
                    target: "llm.anthropic.cache",
                    created = usage.cache_creation_input_tokens,
                    read = usage.cache_read_input_tokens,
                    ratio = usage.cache_hit_ratio(),
                    "llm.anthropic.free call succeeded"
                );
            }
            Err(LlmError::Retryable { reason }) => {
                span.record("outcome", "retryable");
                warn!(reason = %reason, "llm.anthropic.free call failed (retryable)");
            }
            Err(LlmError::Fatal { reason }) => {
                span.record("outcome", "fatal");
                warn!(reason = ?reason, "llm.anthropic.free call failed (fatal)");
            }
        }
        result
    }

    fn model(&self) -> &str {
        &self.config.model
    }
}

impl AnthropicClient {
    async fn send_free_request(
        &self,
        system: &str,
        messages: &[Message],
        model: &str,
    ) -> Result<(String, Usage), LlmError> {
        let url = format!("{}/v1/messages", self.config.api_base_url);
        let mut wire_messages: Vec<AnthropicFreeMessage> = Vec::with_capacity(messages.len());
        for m in messages {
            let role = match m.role {
                crate::message::MessageRole::User => "user",
                crate::message::MessageRole::Assistant => "assistant",
            };
            wire_messages.push(AnthropicFreeMessage {
                role: role.to_string(),
                content: to_wire_content(&m.content)?,
            });
        }
        let body = AnthropicFreeRequest {
            model: model.to_string(),
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            system: build_system_block(system),
            messages: wire_messages,
        };

        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(reqwest_err_to_llm_error)?;

        let status = response.status();
        let body_bytes = response.bytes().await.map_err(|e| LlmError::Retryable {
            reason: format!("failed to read response body: {e}"),
        })?;

        classify_free_response(status, &body_bytes, self.config.max_tokens)
    }

    async fn send_request(
        &self,
        system: &str,
        user: &str,
        tool: &ToolSchema,
        model: &str,
    ) -> Result<(ToolCall, Usage), LlmError> {
        let url = format!("{}/v1/messages", self.config.api_base_url);
        let body = AnthropicRequest {
            model,
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            system: build_system_block(system),
            messages: [AnthropicMessage {
                role: "user",
                content: user,
            }],
            tools: [ToolSchemaWire {
                name: &tool.name,
                description: &tool.description,
                input_schema: &tool.input_schema,
            }],
            tool_choice: ToolChoiceWire {
                kind: "tool",
                name: &tool.name,
            },
        };

        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(reqwest_err_to_llm_error)?;

        let status = response.status();
        let body_bytes = response.bytes().await.map_err(|e| LlmError::Retryable {
            reason: format!("failed to read response body: {e}"),
        })?;

        classify_response(status, &body_bytes, &tool.name, self.config.max_tokens)
    }
}

/// Validate `api_base_url` at construction time so a malformed config
/// value surfaces as `Fatal(ConfigInvalid)` immediately instead of as
/// a misclassified `Retryable` at call time.
///
/// Checks: non-empty, no control characters, must parse as a URL,
/// scheme must be `http` or `https`, host must be present, path must
/// not include a trailing slash (since we concatenate `/v1/messages`
/// at call time).
#[instrument(level = "debug", skip_all, fields(url = url_str), err)]
fn validate_api_base_url(url_str: &str) -> Result<(), LlmError> {
    if url_str.is_empty() {
        return Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid("empty api-base-url".into()),
        });
    }
    if url_str.chars().any(|c| c.is_control()) {
        return Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid("api-base-url contains control characters".into()),
        });
    }
    if url_str.ends_with('/') {
        return Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid("api-base-url must not end with a trailing slash".into()),
        });
    }
    let parsed = url::Url::parse(url_str).map_err(|e| LlmError::Fatal {
        reason: FatalReason::ConfigInvalid(format!("api-base-url not a valid URL: {e}")),
    })?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(LlmError::Fatal {
                reason: FatalReason::ConfigInvalid(format!("api-base-url scheme must be http or https, got {other}")),
            });
        }
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid("api-base-url missing host".into()),
        });
    }
    Ok(())
}

/// Convert a slice of `MessageContent` blocks into the Anthropic wire
/// `content` field. Rules:
/// - Exactly one `Text` block → a bare JSON string (plain string form,
///   matching the existing single-turn wire shape for cache locality).
/// - Any other combination → a JSON array of content-block objects.
/// - `ToolUse`/`ToolResult` blocks → `Fatal(NotImplemented)` until 2.1
///   wires the full Researcher path.
fn to_wire_content(blocks: &[MessageContent]) -> Result<Value, LlmError> {
    if blocks.len() == 1
        && let MessageContent::Text { text } = &blocks[0]
    {
        return Ok(Value::String(text.clone()));
    }
    let mut arr = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            MessageContent::Text { text } => {
                arr.push(serde_json::json!({"type": "text", "text": text}));
            }
            MessageContent::ToolUse { .. } | MessageContent::ToolResult { .. } => {
                return Err(LlmError::Fatal {
                    reason: crate::error::FatalReason::NotImplemented {
                        feature: "ToolUse/ToolResult message content (deferred to 2.1)".into(),
                    },
                });
            }
        }
    }
    Ok(Value::Array(arr))
}

/// Truncate a prompt to `PROMPT_PREVIEW_MAX_BYTES` for span emission,
/// preserving UTF-8 boundaries. Longer prompts get an ellipsis plus
/// the original byte-length suffix so readers of `events.log` know a
/// truncation happened and can find the full prompt in the caller's
/// own records.
fn truncate_preview(s: &str) -> String {
    if s.len() <= PROMPT_PREVIEW_MAX_BYTES {
        return s.to_string();
    }
    let mut cut = PROMPT_PREVIEW_MAX_BYTES;
    while !s.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    format!("{}… [truncated; original {} bytes]", &s[..cut], s.len())
}

/// The `Usage` whose token/cost fields belong on the call span: the
/// success payload's usage, or the billed usage carried on an
/// error-classified 200 (`max_tokens` truncation / schema refusal).
/// `None` for genuine transient/fatal errors that billed nothing.
fn usage_to_record<T>(result: &Result<(T, Usage), LlmError>) -> Option<&Usage> {
    match result {
        Ok((_, usage)) => Some(usage),
        Err(e) => e.billed_usage(),
    }
}

/// Record the per-call token + cost fields on the active span. The
/// `cost_micros` figure uses the model the API actually ran
/// (`usage.model`, falling back to the configured `effective_model`)
/// against the shared digest rate table, so the span and the per-process
/// digest agree on cost. Cache fields are recorded here too (the single
/// place usage lands on the span).
fn record_usage_span(span: &tracing::Span, effective_model: &str, usage: &Usage) {
    let model = usage.model.as_deref().unwrap_or(effective_model);
    let cost = telemetry::digest::cost::cost_micros(
        model,
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
    );
    span.record("input_tokens", usage.input_tokens);
    span.record("output_tokens", usage.output_tokens);
    span.record("cost_micros", cost);
    span.record("model_returned", model);
    span.record("cache_creation_input_tokens", usage.cache_creation_input_tokens);
    span.record("cache_read_input_tokens", usage.cache_read_input_tokens);
    span.record("cache_hit_ratio", usage.cache_hit_ratio());
}

fn reqwest_err_to_llm_error(e: reqwest::Error) -> LlmError {
    // `is_builder()` covers URL parsing and other client-construction
    // failures that happen when the `reqwest::RequestBuilder` is built
    // from a bad URL string — retrying identically will fail
    // identically, so this is Fatal. The URL validation in
    // `AnthropicClient::new` catches the common case at construction
    // time; this branch catches anything that slips past (e.g. a
    // builder failing mid-send for a reason our pre-check missed).
    if e.is_builder() {
        return LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(format!("request builder error: {e}")),
        };
    }
    if e.is_timeout() || e.is_connect() || e.is_request() {
        LlmError::Retryable {
            reason: format!("network error: {e}"),
        }
    } else {
        LlmError::Retryable {
            reason: format!("reqwest error: {e}"),
        }
    }
}

#[instrument(
    name = "llm.anthropic.classify_response",
    level = "debug",
    skip_all,
    fields(status = status.as_u16(), body_bytes = body.len(), expected_tool_name),
    err,
)]
fn classify_response(
    status: reqwest::StatusCode,
    body: &[u8],
    expected_tool_name: &str,
    max_tokens: u32,
) -> Result<(ToolCall, Usage), LlmError> {
    if status.is_success() {
        let parsed: Value = serde_json::from_slice(body).map_err(|_| LlmError::Retryable {
            reason: "non-JSON response body".into(),
        })?;
        return extract_tool_call(&parsed, expected_tool_name, max_tokens);
    }

    let body_text = String::from_utf8_lossy(body).to_string();
    let code = status.as_u16();
    match code {
        401 | 403 => Err(LlmError::Fatal {
            reason: FatalReason::Auth(body_text),
        }),
        408 | 429 | 500..=599 => Err(LlmError::Retryable {
            reason: format!("HTTP {code}: {body_text}"),
        }),
        400 => Err(LlmError::Fatal {
            reason: FatalReason::BadRequest(body_text),
        }),
        _ => Err(LlmError::Fatal {
            reason: FatalReason::BadRequest(format!("HTTP {code}: {body_text}")),
        }),
    }
}

#[instrument(
    name = "llm.anthropic.classify_free_response",
    level = "debug",
    skip_all,
    fields(status = status.as_u16(), body_bytes = body.len()),
    err,
)]
fn classify_free_response(
    status: reqwest::StatusCode,
    body: &[u8],
    max_tokens: u32,
) -> Result<(String, Usage), LlmError> {
    if status.is_success() {
        let parsed: Value = serde_json::from_slice(body).map_err(|_| LlmError::Retryable {
            reason: "non-JSON response body".into(),
        })?;
        return extract_text_block(&parsed, max_tokens);
    }

    let body_text = String::from_utf8_lossy(body).to_string();
    let code = status.as_u16();
    match code {
        401 | 403 => Err(LlmError::Fatal {
            reason: FatalReason::Auth(body_text),
        }),
        408 | 429 | 500..=599 => Err(LlmError::Retryable {
            reason: format!("HTTP {code}: {body_text}"),
        }),
        400 => Err(LlmError::Fatal {
            reason: FatalReason::BadRequest(body_text),
        }),
        _ => Err(LlmError::Fatal {
            reason: FatalReason::BadRequest(format!("HTTP {code}: {body_text}")),
        }),
    }
}

/// Pull the `usage` object out of a Messages-API response and stamp it
/// with the response's top-level `model` field. `serde_json`
/// deserialization defaults missing cache fields to zero (see `Usage`
/// field annotations); `model` is set manually here because the API
/// reports it at the response root, not inside `usage`. The captured
/// `model` is the model-pinning carrier (the concrete id the provider
/// actually ran, which may differ from the floating tag we requested).
fn extract_usage(response: &Value) -> Usage {
    let mut usage = response
        .get("usage")
        .and_then(|v| serde_json::from_value::<Usage>(v.clone()).ok())
        .unwrap_or_default();
    usage.model = response.get("model").and_then(Value::as_str).map(str::to_string);
    usage
}

/// Extract the first `{"type": "text"}` content block from a
/// Messages-API response. Thinking blocks (`{"type": "thinking"}`)
/// are skipped at debug level; they appear when the model's
/// extended-thinking mode is enabled upstream and we do not surface
/// that content to the Implementer.
fn extract_text_block(response: &Value, max_tokens: u32) -> Result<(String, Usage), LlmError> {
    if response.get("stop_reason").and_then(Value::as_str) == Some("max_tokens") {
        // The provider billed the full truncated response; carry the
        // usage on the error so the metered client records the cost of
        // this expensive failure on its error path (Phase 1 remediation).
        let usage = extract_usage(response);
        let used = usage.output_tokens as u32;
        return Err(LlmError::Fatal {
            reason: FatalReason::ContextExhausted {
                used,
                limit: max_tokens,
                usage,
            },
        });
    }

    let usage = extract_usage(response);

    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Fatal {
            reason: FatalReason::SchemaValidation {
                message: "response missing `content` array".into(),
                usage: usage.clone(),
            },
        })?;

    for block in content {
        let kind = block.get("type").and_then(Value::as_str);
        match kind {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LlmError::Fatal {
                        reason: FatalReason::SchemaValidation {
                            message: "text block missing `text` field".into(),
                            usage: usage.clone(),
                        },
                    })?;
                return Ok((text.to_string(), usage));
            }
            Some("thinking") => {
                debug!("llm.anthropic.free: thinking block discarded");
                continue;
            }
            _ => continue,
        }
    }

    Err(LlmError::Fatal {
        reason: FatalReason::SchemaValidation {
            message: "no text content block in response".into(),
            usage: usage.clone(),
        },
    })
}

fn extract_tool_call(
    response: &Value,
    expected_tool_name: &str,
    max_tokens: u32,
) -> Result<(ToolCall, Usage), LlmError> {
    if response.get("stop_reason").and_then(Value::as_str) == Some("max_tokens") {
        // The provider billed the full truncated response; carry the
        // usage on the error so the metered client records the cost of
        // this expensive failure on its error path (Phase 1 remediation).
        let usage = extract_usage(response);
        let used = usage.output_tokens as u32;
        return Err(LlmError::Fatal {
            reason: FatalReason::ContextExhausted {
                used,
                limit: max_tokens,
                usage,
            },
        });
    }

    let usage = extract_usage(response);

    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Fatal {
            reason: FatalReason::SchemaValidation {
                message: "response missing `content` array".into(),
                usage: usage.clone(),
            },
        })?;

    for block in content {
        let kind = block.get("type").and_then(Value::as_str);
        if kind != Some("tool_use") {
            continue;
        }
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if name != expected_tool_name {
            continue;
        }
        let input = block.get("input").ok_or_else(|| LlmError::Fatal {
            reason: FatalReason::SchemaValidation {
                message: "tool_use block missing `input` field".into(),
                usage: usage.clone(),
            },
        })?;
        if !input.is_object() && !input.is_array() {
            return Err(LlmError::Fatal {
                reason: FatalReason::SchemaValidation {
                    message: format!("tool_use input unparseable: expected object or array, got {input}"),
                    usage: usage.clone(),
                },
            });
        }
        return Ok((
            ToolCall {
                tool_name: name,
                input: input.clone(),
            },
            usage,
        ));
    }

    Err(LlmError::Fatal {
        reason: FatalReason::SchemaValidation {
            message: "no tool-use content block".into(),
            usage: usage.clone(),
        },
    })
}
