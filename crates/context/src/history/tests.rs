#![allow(clippy::unwrap_used)]

use llm::{Message, MessageContent};
use serde_json::json;

use super::{estimate_message_tokens, trim_history};

/// Pad `t` with enough repetition to produce a non-zero token estimate.
/// All test messages use this so token estimates are predictable.
fn user(t: &str) -> Message {
    Message::user(format!("{} {}", t, "x".repeat(40)))
}

fn asst(t: &str) -> Message {
    Message::assistant(format!("{} {}", t, "x".repeat(40)))
}

fn asst_tool_use(id: &str, name: &str) -> Message {
    Message {
        role: llm::MessageRole::Assistant,
        content: vec![MessageContent::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: json!({"cmd": "ls"}),
        }],
    }
}

fn user_tool_result(tool_use_id: &str) -> Message {
    Message::tool_result(tool_use_id, "result text")
}

#[test]
fn budget_zero_returns_empty() {
    let history = vec![user("hello"), asst("world")];
    assert!(trim_history(&history, 0).is_empty());
}

#[test]
fn budget_large_returns_all() {
    let history = vec![user("hello"), asst("world"), user("again"), asst("ok")];
    let result = trim_history(&history, 1_000_000);
    assert_eq!(result.len(), 4);
}

#[test]
fn empty_history_returns_empty() {
    let result = trim_history(&[], 100);
    assert!(result.is_empty());
}

#[test]
fn two_message_arc_dropped_atomically() {
    // [user0, asst0, user1, asst1] — budget forces dropping oldest arc
    let history = vec![user("q0"), asst("a0"), user("q1"), asst("a1")];
    // Token cost of first two messages must exceed this budget
    let keep_arc_tokens = estimate_message_tokens(&history[2]) + estimate_message_tokens(&history[3]);
    // Budget: keep second arc but not first
    let budget = keep_arc_tokens + 1;
    let result = trim_history(&history, budget);
    assert_eq!(result.len(), 2, "should keep only second arc; got {result:?}");
    assert!(matches!(result[0].role, llm::MessageRole::User));
}

#[test]
fn four_message_tool_arc_dropped_atomically() {
    // [User(prompt), Asst(ToolUse), User(ToolResult), Asst(text), User(q1), Asst(a1)]
    let history = vec![
        user("prompt0"),
        asst_tool_use("tu1", "read"),
        user_tool_result("tu1"),
        asst("final answer"),
        user("q1"),
        asst("a1"),
    ];
    let first_arc_tokens: usize = history[0..4].iter().map(estimate_message_tokens).sum();
    let second_arc_tokens: usize = history[4..6].iter().map(estimate_message_tokens).sum();
    let budget = second_arc_tokens + 1;
    assert!(first_arc_tokens > 0);
    let result = trim_history(&history, budget);
    assert_eq!(result.len(), 2, "should drop the 4-message arc; got {result:?}");
    // Remaining history must start with a fresh User message (not ToolResult)
    assert!(!result[0].has_tool_result());
}

#[test]
fn six_message_sequential_tool_arc_dropped_atomically() {
    // [User, Asst(TU1), User(TR1), Asst(TU2), User(TR2), Asst(final), User(q1), Asst(a1)]
    let history = vec![
        user("prompt0"),
        asst_tool_use("tu1", "search"),
        user_tool_result("tu1"),
        asst_tool_use("tu2", "read"),
        user_tool_result("tu2"),
        asst("final"),
        user("q1"),
        asst("a1"),
    ];
    let first_arc_tokens: usize = history[0..6].iter().map(estimate_message_tokens).sum();
    let second_arc_tokens: usize = history[6..8].iter().map(estimate_message_tokens).sum();
    let budget = second_arc_tokens + 1;
    assert!(first_arc_tokens > 0);
    let result = trim_history(&history, budget);
    assert_eq!(result.len(), 2, "should drop 6-message arc atomically; got {result:?}");
    assert!(!result[0].has_tool_result());
}

#[test]
fn mixed_arc_sizes_oldest_dropped_first() {
    // 2-message arc (oldest) + 4-message arc (newer)
    let history = vec![
        user("short q"),
        asst("short a"),
        user("prompt"),
        asst_tool_use("tu1", "read"),
        user_tool_result("tu1"),
        asst("final"),
    ];
    let first_arc_tokens: usize = history[0..2].iter().map(estimate_message_tokens).sum();
    let second_arc_tokens: usize = history[2..6].iter().map(estimate_message_tokens).sum();
    // Budget: fit 4-message arc but not 2-message + 4-message together
    let budget = second_arc_tokens + 1;
    assert!(first_arc_tokens > 0);
    let result = trim_history(&history, budget);
    assert_eq!(result.len(), 4, "should drop 2-msg arc, keep 4-msg arc; got {result:?}");
    assert!(!result[0].has_tool_result());
}

#[test]
fn tool_use_input_uses_json_serialized_length() {
    let large_input = json!({"key": "x".repeat(400)});
    let msg = Message {
        role: llm::MessageRole::Assistant,
        content: vec![MessageContent::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input: large_input.clone(),
        }],
    };
    let expected = serde_json::to_string(&large_input).unwrap().len() / super::CHARS_PER_TOKEN;
    assert_eq!(estimate_message_tokens(&msg), expected);
}
