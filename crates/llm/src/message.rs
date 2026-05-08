//! `Message`: the multi-turn conversation unit consumed by
//! `LlmClient::complete_free`.
//!
//! Lives in `llm` (not `agents`) because `LlmClient::complete_free`
//! consumes `&[Message]` in its public trait signature. `llm` depends
//! only on `domain` + `telemetry`; `agents` depends on `llm`.
//! Putting `Message` in `agents` would create a `llm → agents` cycle.
//!
//! `MessageContent` variants:
//! - `Text` — plain text, the only variant used by Director and
//!   Implementer self-correction loops.
//! - `ToolUse` / `ToolResult` — typed stubs for the Researcher's
//!   tool-calling arc (2.1). `AnthropicClient` returns
//!   `Fatal(NotImplemented)` if it encounters them until 2.1 wires the
//!   full path.

use serde::{Deserialize, Serialize};

/// A single turn in a multi-turn conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<MessageContent>,
}

/// Conversation participant. Serialises as `"user"` / `"assistant"` to
/// match the Anthropic Messages API wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
}

/// The payload of a single content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    /// Plain text. Used by Director (state summaries), Implementer
    /// self-correction, and Reviewer. Named field `text` matches the
    /// Anthropic wire format and satisfies serde's internal-tag
    /// requirement (a map, not a bare string).
    Text { text: String },
    /// A tool-invocation request emitted by the assistant. Defined
    /// now for type completeness; `AnthropicClient` returns
    /// `Fatal(NotImplemented)` until 2.1 activates this path.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The result of a tool invocation, returned by the user turn.
    /// Paired with a preceding `ToolUse` block. Same staged-stub rule.
    ToolResult { tool_use_id: String, content: String },
}

impl Message {
    /// Construct a user turn with a single `Text` block.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: vec![MessageContent::Text { text: text.into() }],
        }
    }

    /// Construct an assistant turn with a single `Text` block.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: vec![MessageContent::Text { text: text.into() }],
        }
    }

    /// Construct a user turn carrying a single `ToolResult` block.
    /// The `tool_use_id` must match the `id` on the preceding
    /// `ToolUse` block in the assistant turn.
    pub fn tool_result(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: vec![MessageContent::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
            }],
        }
    }

    /// Returns `true` if this message's content contains any
    /// `ToolResult` block. Used by `trim_history` to identify the
    /// boundary between Logical Turn Arcs.
    pub fn has_tool_result(&self) -> bool {
        self.content
            .iter()
            .any(|c| matches!(c, MessageContent::ToolResult { .. }))
    }
}
