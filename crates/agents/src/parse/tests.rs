#![allow(clippy::unwrap_used)]

use super::{ParseError, parse_actions, parse_one};
use crate::action::AgentAction;

#[test]
fn parses_single_action_array() {
    let input = r#"[{"type":"done","message":"nothing to do"}]"#;
    let actions = parse_actions(input).unwrap();
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        AgentAction::Done { message } => assert_eq!(message, "nothing to do"),
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn parses_all_five_variants() {
    let input = r#"[
        {"type":"run_tool","tool":"bash","input":{"command":"ls"}},
        {"type":"commit_changes","message":"wip"},
        {"type":"propose_bundle","claims":["it compiles"]},
        {"type":"done","message":"no-op"},
        {"type":"need_help","reason":"stuck"}
    ]"#;
    let actions = parse_actions(input).unwrap();
    assert_eq!(actions.len(), 5);
    assert!(matches!(actions[0], AgentAction::RunTool { .. }));
    assert!(matches!(actions[1], AgentAction::CommitChanges { .. }));
    assert!(matches!(actions[2], AgentAction::ProposeBundle { .. }));
    assert!(matches!(actions[3], AgentAction::Done { .. }));
    assert!(matches!(actions[4], AgentAction::NeedHelp { .. }));
}

#[test]
fn strips_json_markdown_fence() {
    let input = "```json\n[{\"type\":\"done\",\"message\":\"ok\"}]\n```";
    let actions = parse_actions(input).unwrap();
    assert_eq!(actions.len(), 1);
}

#[test]
fn strips_bare_markdown_fence() {
    let input = "```\n[{\"type\":\"done\",\"message\":\"ok\"}]\n```";
    let actions = parse_actions(input).unwrap();
    assert_eq!(actions.len(), 1);
}

#[test]
fn normalizes_action_key_to_type() {
    // The LLM sometimes emits the v3 key name `"action"`; we
    // normalize it to `"type"` before deserializing.
    let input = r#"[{"action":"done","message":"hi"}]"#;
    let actions = parse_actions(input).unwrap();
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], AgentAction::Done { message } if message == "hi"));
}

#[test]
fn extracts_array_from_surrounding_prose() {
    let input = r#"Sure! Here's what I'll do: [{"type":"done","message":"ok"}] - let me know if you want changes."#;
    let actions = parse_actions(input).unwrap();
    assert_eq!(actions.len(), 1);
}

#[test]
fn balanced_bracket_ignores_brackets_in_prose() {
    // The closing bracket of the real JSON is at a different offset
    // than `rfind(']')` would find. A prior approach using rfind
    // would grab the wrong position and fail.
    let input = r#"The result [is good]: [{"type":"done","message":"ok"}] and that's it ]"#;
    let actions = parse_actions(input).unwrap();
    assert_eq!(actions.len(), 1);
}

#[test]
fn balanced_bracket_ignores_brackets_in_strings() {
    // Brackets inside a JSON string literal must not confuse the
    // balancer. Here the tool input has "[literal]" as a value.
    let input = r#"[{"type":"run_tool","tool":"bash","input":{"command":"echo [literal]"}}]"#;
    let actions = parse_actions(input).unwrap();
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        AgentAction::RunTool { tool, input } => {
            assert_eq!(tool, "bash");
            assert_eq!(input["command"], "echo [literal]");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn handles_nested_arrays_in_tool_input() {
    let input = r#"[{"type":"run_tool","tool":"x","input":{"nested":[1,2,[3,4]]}}]"#;
    let actions = parse_actions(input).unwrap();
    assert_eq!(actions.len(), 1);
}

#[test]
fn empty_array_returns_empty_array_error() {
    let input = "[]";
    let err = parse_actions(input).unwrap_err();
    assert!(matches!(err, ParseError::EmptyArray));
}

#[test]
fn malformed_json_returns_error() {
    let input = "[{type:done";
    let err = parse_actions(input).unwrap_err();
    // Either Serde (extract found but parse failed) or NoArrayFound
    // (no balanced bracket).
    assert!(
        matches!(err, ParseError::Serde(_) | ParseError::NoArrayFound),
        "unexpected: {err:?}"
    );
}

#[test]
fn prose_with_no_array_returns_no_array_found() {
    let input = "I'm not sure what to do, please help.";
    let err = parse_actions(input).unwrap_err();
    assert!(matches!(err, ParseError::NoArrayFound));
}

#[test]
fn unknown_type_returns_serde_error() {
    let input = r#"[{"type":"teleport","target":"mars"}]"#;
    let err = parse_actions(input).unwrap_err();
    assert!(matches!(err, ParseError::Serde(_)));
}

#[test]
fn parse_one_accepts_single_object() {
    let input = r#"{"type":"done","message":"ok"}"#;
    let action = parse_one(input).unwrap();
    assert!(matches!(action, AgentAction::Done { .. }));
}

#[test]
fn parse_one_falls_back_to_first_of_array() {
    // The correctable-error re-prompt asks for a single corrected
    // action; if the LLM emits an array anyway, take element 0.
    let input = r#"[{"type":"done","message":"a"},{"type":"done","message":"b"}]"#;
    let action = parse_one(input).unwrap();
    match action {
        AgentAction::Done { message } => assert_eq!(message, "a"),
        other => panic!("unexpected: {other:?}"),
    }
}
