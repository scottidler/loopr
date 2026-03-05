use serde::{Deserialize, Serialize};

/// Tool definition exported for the Anthropic API `tools` array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A tool call from the LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Content block types in an Anthropic API response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// Stop reason from the Anthropic API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

/// A message in the Anthropic API conversation format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

/// Response from the Anthropic completions API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub id: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub model: String,
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_definition_serde() {
        let def = ToolDefinition {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let json = serde_json::to_string(&def).unwrap();
        let deserialized: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "read");
    }

    #[test]
    fn test_tool_call_serde() {
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "test".to_string(),
            input: serde_json::json!({"args": ["--verbose"]}),
        };
        let json = serde_json::to_string(&call).unwrap();
        let deserialized: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "test");
        assert_eq!(deserialized.id, "call-1");
    }

    #[test]
    fn test_content_block_text() {
        let block = ContentBlock::Text {
            text: "hello".to_string(),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"text\""));
    }

    #[test]
    fn test_content_block_tool_use() {
        let block = ContentBlock::ToolUse {
            id: "tu-1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({"path": "src/main.rs"}),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"tool_use\""));
        let deserialized: ContentBlock = serde_json::from_str(&json).unwrap();
        match deserialized {
            ContentBlock::ToolUse { id, name, .. } => {
                assert_eq!(id, "tu-1");
                assert_eq!(name, "read");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_stop_reason_serde() {
        let reason = StopReason::ToolUse;
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, "\"tool_use\"");
        let deserialized: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, StopReason::ToolUse);
    }

    #[test]
    fn test_message_serde() {
        let msg = Message {
            role: "assistant".to_string(),
            content: vec![ContentBlock::Text { text: "hi".to_string() }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.role, "assistant");
        assert_eq!(deserialized.content.len(), 1);
    }
}
