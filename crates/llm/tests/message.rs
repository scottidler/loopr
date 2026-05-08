//! Serde round-trip tests for Message / MessageContent / MessageRole.

use llm::{Message, MessageContent, MessageRole};
use serde_json::{Value, json};

#[test]
fn message_role_user_serialises_as_lowercase_string() {
    let v = serde_json::to_value(MessageRole::User).unwrap();
    assert_eq!(v, json!("user"));
}

#[test]
fn message_role_assistant_serialises_as_lowercase_string() {
    let v = serde_json::to_value(MessageRole::Assistant).unwrap();
    assert_eq!(v, json!("assistant"));
}

#[test]
fn message_role_roundtrips() {
    for role in [MessageRole::User, MessageRole::Assistant] {
        let s = serde_json::to_string(&role).unwrap();
        let back: MessageRole = serde_json::from_str(&s).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), s);
    }
}

#[test]
fn message_content_text_roundtrip() {
    let c = MessageContent::Text {
        text: "hello world".to_string(),
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["type"], json!("text"));
    assert_eq!(v["text"], json!("hello world"));
    let back: MessageContent = serde_json::from_value(v).unwrap();
    assert!(matches!(back, MessageContent::Text { text } if text == "hello world"));
}

#[test]
fn message_content_tool_use_roundtrip() {
    let c = MessageContent::ToolUse {
        id: "tu_abc".to_string(),
        name: "bash".to_string(),
        input: json!({"cmd": "ls -la"}),
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["type"], json!("tool_use"));
    assert_eq!(v["id"], json!("tu_abc"));
    assert_eq!(v["name"], json!("bash"));
    let back: MessageContent = serde_json::from_value(v).unwrap();
    assert!(matches!(back, MessageContent::ToolUse { id, name, .. } if id == "tu_abc" && name == "bash"));
}

#[test]
fn message_content_tool_result_roundtrip() {
    let c = MessageContent::ToolResult {
        tool_use_id: "tu_abc".to_string(),
        content: "file.rs: ok".to_string(),
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["type"], json!("tool_result"));
    assert_eq!(v["tool_use_id"], json!("tu_abc"));
    let back: MessageContent = serde_json::from_value(v).unwrap();
    assert!(matches!(back, MessageContent::ToolResult { tool_use_id, .. } if tool_use_id == "tu_abc"));
}

#[test]
fn message_user_constructor_produces_single_text_block() {
    let m = Message::user("test");
    assert!(matches!(m.role, MessageRole::User));
    assert_eq!(m.content.len(), 1);
    assert!(matches!(&m.content[0], MessageContent::Text { text } if text == "test"));
}

#[test]
fn message_assistant_constructor_produces_single_text_block() {
    let m = Message::assistant("reply");
    assert!(matches!(m.role, MessageRole::Assistant));
    assert_eq!(m.content.len(), 1);
    assert!(matches!(&m.content[0], MessageContent::Text { text } if text == "reply"));
}

#[test]
fn message_tool_result_constructor_produces_tool_result_block() {
    let m = Message::tool_result("tu_1", "output text");
    assert!(matches!(m.role, MessageRole::User));
    assert!(
        matches!(&m.content[0], MessageContent::ToolResult { tool_use_id, content }
        if tool_use_id == "tu_1" && content == "output text")
    );
}

#[test]
fn has_tool_result_returns_false_for_text_message() {
    assert!(!Message::user("hello").has_tool_result());
    assert!(!Message::assistant("world").has_tool_result());
}

#[test]
fn has_tool_result_returns_true_for_tool_result_message() {
    assert!(Message::tool_result("tu_1", "ok").has_tool_result());
}

#[test]
fn message_full_roundtrip_json() {
    let m = Message::user("prompt text");
    let v: Value = serde_json::to_value(&m).unwrap();
    assert_eq!(v["role"], json!("user"));
    let back: Message = serde_json::from_value(v).unwrap();
    assert!(matches!(back.role, MessageRole::User));
}
