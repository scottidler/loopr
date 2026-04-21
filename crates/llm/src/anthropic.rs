//! `AnthropicClient`: the one concrete `LlmClient` impl shipped in
//! Stage 6. Talks to Anthropic's Messages API with `tool_choice`
//! locked to a caller-supplied tool. No SSE, no multi-turn, no retry.
//!
//! Seam layering matches the trait's contract: one buffered HTTP call
//! per invocation; the status code and response shape are classified
//! into `LlmError` variants per the table in the design doc; the
//! first matching `tool_use` content block becomes the returned
//! `ToolCall`.

use std::future::Future;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::Value;

use crate::client::LlmClient;
use crate::config::LlmConfig;
use crate::error::{FatalReason, LlmError};
use crate::tool::{ToolCall, ToolSchema};

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
    pub fn new(config: LlmConfig, api_key: String) -> Result<Self, LlmError> {
        if api_key.is_empty() {
            return Err(LlmError::Fatal {
                reason: FatalReason::ConfigInvalid("empty API key".into()),
            });
        }

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
#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    temperature: f32,
    system: &'a str,
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

impl LlmClient for AnthropicClient {
    #[allow(clippy::manual_async_fn)] // explicit `+ Send` required; see trait
    fn complete_with_tool<'a>(
        &'a self,
        system: &'a str,
        user: &'a str,
        tool: ToolSchema,
    ) -> impl Future<Output = Result<ToolCall, LlmError>> + Send + 'a {
        async move {
            let url = format!("{}/v1/messages", self.config.api_base_url);
            let body = AnthropicRequest {
                model: &self.config.model,
                max_tokens: self.config.max_tokens,
                temperature: self.config.temperature,
                system,
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
}

fn reqwest_err_to_llm_error(e: reqwest::Error) -> LlmError {
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

fn classify_response(
    status: reqwest::StatusCode,
    body: &[u8],
    expected_tool_name: &str,
    max_tokens: u32,
) -> Result<ToolCall, LlmError> {
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

fn extract_tool_call(response: &Value, expected_tool_name: &str, max_tokens: u32) -> Result<ToolCall, LlmError> {
    if response.get("stop_reason").and_then(Value::as_str) == Some("max_tokens") {
        let used = response
            .get("usage")
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        return Err(LlmError::Fatal {
            reason: FatalReason::ContextExhausted {
                used,
                limit: max_tokens,
            },
        });
    }

    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Fatal {
            reason: FatalReason::SchemaValidation("response missing `content` array".into()),
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
            reason: FatalReason::SchemaValidation("tool_use block missing `input` field".into()),
        })?;
        if !input.is_object() && !input.is_array() {
            return Err(LlmError::Fatal {
                reason: FatalReason::SchemaValidation(format!(
                    "tool_use input unparseable: expected object or array, got {input}"
                )),
            });
        }
        return Ok(ToolCall {
            tool_name: name,
            input: input.clone(),
        });
    }

    Err(LlmError::Fatal {
        reason: FatalReason::SchemaValidation("no tool-use content block".into()),
    })
}
