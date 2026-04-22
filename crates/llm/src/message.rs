//! `ChatMessage`: multi-turn conversation unit for `complete_free`.
//!
//! Lives in `llm` (not `agents`) because `LlmClient::complete_free`
//! consumes `&[ChatMessage]` in its public trait signature. `llm`
//! depends only on `domain` + `telemetry`; `agents` depends on `llm`.
//! Putting `ChatMessage` in `agents` would require `llm → agents`,
//! which is a cycle.

use serde::{Deserialize, Serialize};

/// A single turn in a multi-turn conversation. Only `user` and
/// `assistant` roles are represented; the system prompt is passed
/// separately to `complete_free`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    /// User-role message. Constructor pins the role string so
    /// callers don't typo it at every site.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    /// Assistant-role message. The Implementer's self-correction
    /// loop echoes the LLM's failing response back into the history
    /// under this role so the next call sees the full context.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}
