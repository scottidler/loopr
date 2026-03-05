use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

/// Slash command tool — allows the LLM to invoke slash commands (e.g., /commit, /status).
pub struct SlashTool;

#[async_trait]
impl Tool for SlashTool {
    fn name(&self) -> &str {
        "slash"
    }

    fn description(&self) -> &str {
        "Invoke a slash command (e.g., /commit, /status)"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Slash command name (without the /)"
                },
                "args": {
                    "type": "string",
                    "description": "Arguments for the slash command"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let command = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult {
                    content: "missing required parameter: command".into(),
                    is_error: true,
                };
            }
        };

        // Placeholder — slash commands will be routed through the TUI/agent system
        ToolResult {
            content: format!("slash command '{}' acknowledged (not yet implemented)", command),
            is_error: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_slash_tool() {
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "test".into());
        let result = SlashTool.execute(json!({"command": "status"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("status"));
    }
}
