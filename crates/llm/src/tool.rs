//! Tool-call surface crossing the `LlmClient` trait boundary.
//!
//! `ToolSchema` carries a caller-supplied JSON Schema; `ToolCall`
//! carries the extracted tool-use response. These are intentionally
//! JSON-primitive-shaped (no `tools`-crate types) because `llm` cannot
//! depend on `tools` without recoupling network and subprocess
//! concerns. See `crates/llm/CLAUDE.md` for the "we own how we talk to
//! an LLM, not what we say" rule.

use serde::Serialize;

/// A tool definition the caller wants the model to invoke. The shape
/// matches Anthropic's `tools` array element: `{name, description,
/// input_schema}`. The schema is `serde_json::Value` rather than a
/// typed struct so the decomposer can ship any JSON Schema the
/// Anthropic API accepts without cluttering `llm` with caller-specific
/// shape. Validation that the returned `ToolCall.input` matches this
/// schema is the caller's job; `llm` reports only "tool-use block
/// present and well-formed JSON" vs. "absent or unparseable" as
/// `FatalReason::SchemaValidation`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// The model's tool-use response, extracted from the first `tool_use`
/// content block whose `name` matches the caller's request. `input`
/// is opaque JSON; the caller's schema-specific code parses it.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_name: String,
    pub input: serde_json::Value,
}
