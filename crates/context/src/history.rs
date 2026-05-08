//! History-trimming utility for multi-turn context assembly.
//!
//! `trim_history` drops the oldest Logical Turn Arcs from a message
//! history until the remaining turns fit within a token budget. It
//! preserves Anthropic's strict alternating-role invariant and never
//! splits a tool-use arc.
//!
//! # Logical Turn Arc
//!
//! An arc begins at any `User` message whose content does NOT include
//! `ToolResult` blocks (i.e. a fresh prompt, not a tool response). The
//! arc extends through all subsequent messages — including any number of
//! `[Assistant(ToolUse), User(ToolResult)]` sub-cycles — up to and
//! including the final `Assistant` message before the next fresh `User`
//! message.
//!
//! Examples:
//! - Plain-text exchange: `[User(text), Asst(text)]` — 2 messages.
//! - Single tool call: `[User(prompt), Asst(TU), User(TR), Asst(text)]` — 4 msgs.
//! - Two sequential tool calls: 6 messages.
//!
//! `trim_history` drops the oldest complete arc; it never drops a
//! partial arc.

use llm::{Message, MessageContent};

use crate::implementer::CHARS_PER_TOKEN;

/// Estimate the token cost of a single `Message` using the same
/// `chars / CHARS_PER_TOKEN` heuristic as `InlineContextBuilder`.
pub fn estimate_message_tokens(msg: &Message) -> usize {
    msg.content
        .iter()
        .map(|c| match c {
            MessageContent::Text { text } => text.len(),
            MessageContent::ToolUse { input, .. } => serde_json::to_string(input).map(|s| s.len()).unwrap_or(0),
            MessageContent::ToolResult { content, .. } => content.len(),
        })
        .sum::<usize>()
        / CHARS_PER_TOKEN
}

/// Find the end index (exclusive) of the first Logical Turn Arc
/// starting at `start` within `history`. Returns `None` if `start`
/// is out-of-bounds or if `history[start]` is not a fresh User turn.
///
/// A "fresh User turn" is a User message that contains no `ToolResult`
/// blocks. The arc ends just before the next fresh User message (or
/// at the end of the slice if there is no next fresh User message).
fn arc_end(history: &[Message], start: usize) -> Option<usize> {
    if start >= history.len() {
        return None;
    }
    // The arc must begin at a fresh User message.
    let first = &history[start];
    if !matches!(first.role, llm::MessageRole::User) || first.has_tool_result() {
        return None;
    }
    // Scan forward to find the next fresh User message.
    let mut end = start + 1;
    while end < history.len() {
        let m = &history[end];
        if matches!(m.role, llm::MessageRole::User) && !m.has_tool_result() {
            break;
        }
        end += 1;
    }
    Some(end)
}

/// Trim `history` to fit within `budget` tokens by dropping the oldest
/// Logical Turn Arcs first. Never drops a partial arc.
///
/// If the entire history exceeds the budget, returns an empty `Vec`.
/// Callers that receive an empty result must warn and proceed with
/// system + state message only (per the design doc's budget-exhaustion
/// contract).
pub fn trim_history(history: &[Message], budget: usize) -> Vec<Message> {
    // Start with the full history and drop arcs from the front.
    let mut start = 0;
    let mut total_tokens: usize = history.iter().map(estimate_message_tokens).sum();

    while total_tokens > budget && start < history.len() {
        let end = match arc_end(history, start) {
            Some(e) => e,
            // Can't find a clean arc boundary — stop trying to trim further
            // rather than leaving the history in an invalid state.
            None => break,
        };
        // Subtract the tokens for the dropped arc.
        for msg in &history[start..end] {
            total_tokens = total_tokens.saturating_sub(estimate_message_tokens(msg));
        }
        start = end;
    }

    history[start..].to_vec()
}

#[cfg(test)]
mod tests;
