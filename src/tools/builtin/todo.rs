use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

/// Todo tracking tool — allows the LLM to track tasks during execution.
pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Track todo items during execution"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "complete", "list"],
                    "description": "Action to perform on todo list"
                },
                "text": {
                    "type": "string",
                    "description": "Todo item text (for 'add' action)"
                },
                "index": {
                    "type": "integer",
                    "description": "Todo item index (for 'complete' action)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let action = match input.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return ToolResult {
                    content: "missing required parameter: action".into(),
                    is_error: true,
                };
            }
        };

        // Placeholder — todo state will be managed by the agentic loop context
        ToolResult {
            content: format!("todo action '{}' acknowledged (not yet implemented)", action),
            is_error: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_todo_tool() {
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "test".into());
        let result = TodoTool.execute(json!({"action": "list"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("list"));
    }
}
