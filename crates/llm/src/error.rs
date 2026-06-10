//! Typed errors surfaced by `LlmClient` implementations.
//!
//! The Retryable / Fatal split encodes *invariants*, not *policy*:
//! "retrying this same call with this same config will almost certainly
//! fail" is an invariant the caller cannot learn by inspecting
//! `stop_reason` or scraping error body strings. Retry counts and
//! backoff shape remain caller-owned (the decomposer picks a
//! `RetryStrategy`); the taxonomy here just surfaces the
//! what-retrying-achieves signal in a form callers can `match` on.

use crate::usage::Usage;

/// Errors returned by `LlmClient::complete_with_tool`.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Transient condition. The caller's retry policy may re-issue the
    /// same call and reasonably expect a different result: rate limits,
    /// 5xx, network timeouts, non-JSON bodies from broken proxies.
    #[error("retryable LLM call failure: {reason}")]
    Retryable { reason: String },

    /// Permanent condition for this call shape. Retrying identically
    /// will fail identically. The caller must either change inputs
    /// (larger `max_tokens`, different schema) or abort.
    #[error("fatal LLM call failure: {reason:?}")]
    Fatal { reason: FatalReason },
}

impl LlmError {
    /// Token usage the provider billed on an error-classified 200
    /// response. `max_tokens` truncation (`ContextExhausted`) and a
    /// tool-contract refusal / malformed-content body
    /// (`SchemaValidation`) are HTTP 200s that cost tokens but
    /// short-circuit before the success path's metering. The metered
    /// client records this on the error path so the cost counters see
    /// the expensive failures (Phase 1 of the code-review remediation).
    pub fn billed_usage(&self) -> Option<&Usage> {
        match self {
            LlmError::Fatal { reason } => match reason {
                FatalReason::ContextExhausted { usage, .. } => Some(usage),
                FatalReason::SchemaValidation { usage, .. } => Some(usage),
                _ => None,
            },
            LlmError::Retryable { .. } => None,
        }
    }
}

/// Typed causes for `LlmError::Fatal`. Each variant names a class of
/// failure the caller might handle differently (e.g., the decomposer
/// uses `SchemaValidation` as its signal to try the text-parse
/// fallback path).
#[derive(Debug)]
pub enum FatalReason {
    /// 401 / 403 from the provider. API key missing, invalid, or revoked.
    Auth(String),

    /// 400 with an `invalid_request_error` or model-side refusal.
    BadRequest(String),

    /// The response's content array had no `tool_use` block, or the
    /// block's `input` field wasn't parseable JSON. This captures
    /// STRUCTURAL problems only; semantic validation (required fields,
    /// field types matching the caller's JSON Schema) is the caller's
    /// job, not `llm`'s. The decomposer uses this variant as the
    /// signal to try its fallback text-parse path.
    ///
    /// Classified `Fatal` on purpose (Architect round 1 flagged the
    /// alternative): when `tool_choice: {type: "tool", name: "..."}`
    /// is set but the response has no `tool_use` block, the model
    /// has explicitly declined the tool contract; re-running the
    /// identical prompt is rerunning-for-its-own-sake, not recovery.
    /// "input not valid JSON" from Anthropic is extraordinarily rare
    /// and more likely a broken proxy than transient model noise.
    /// If real-world data shows transient JSON malformations at
    /// temperature 0.3 warrant a blind retry, split this into
    /// `SchemaValidation` (Fatal, no tool_use block) and
    /// `SchemaMalformed` (Retryable, unparseable JSON) then.
    SchemaValidation { message: String, usage: Usage },

    /// The model's response was truncated at `max_tokens` before
    /// producing a complete tool-input payload. `used` is
    /// `usage.output_tokens` from the Anthropic response; `limit` is
    /// the `max_tokens` value the caller configured. Marked Fatal (not
    /// Retryable) because retrying with the SAME config will fail
    /// identically. A caller that chooses to retry with a LARGER
    /// `max_tokens` can do so explicitly.
    ContextExhausted { used: u32, limit: u32, usage: Usage },

    /// Config was missing the API key, or the key env var was unset,
    /// or the base URL was malformed.
    ConfigInvalid(String),

    /// A code path that is type-level-defined but not yet wired for
    /// production. Returned by `AnthropicClient` when it encounters
    /// `ToolUse`/`ToolResult` message content before the 2.1
    /// Researcher ships. Encountering this is always a caller error.
    NotImplemented { feature: String },
}
