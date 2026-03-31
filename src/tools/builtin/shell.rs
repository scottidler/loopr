use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::lane::classify;
use crate::tools::shell::{execute_in_lane, format_shell_output};
use crate::tools::traits::{Tool, ToolResult};

/// Generic shell tool for ad-hoc commands. Higher risk — the LLM controls the full command string.
pub struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 120)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let command = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult {
                    content: "missing required parameter: command".into(),
                    is_error: true,
                };
            }
        };
        let timeout = input.get("timeout").and_then(|v| v.as_u64()).unwrap_or(120);

        let lane = classify("shell");
        match execute_in_lane(command, &ctx.working_dir, lane, timeout, &ctx.router).await {
            Ok(output) => ToolResult {
                content: format_shell_output(&output),
                is_error: output.exit_code != 0,
            },
            Err(e) => ToolResult {
                content: format!("shell command failed: {}", e),
                is_error: true,
            },
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_shell_tool() {
        let dir = std::env::temp_dir();
        let ctx = ToolContext::new(dir, "test".into());
        let result = ShellTool.execute(json!({"command": "echo shell-test"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("shell-test"));
    }
}
