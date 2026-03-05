use async_trait::async_trait;
use serde_json::json;

use crate::config::ToolEntry;
use crate::tools::context::ToolContext;
use crate::tools::shell::{execute_shell_command, format_shell_output};
use crate::tools::traits::{Tool, ToolResult};

/// A configured project tool (from `loopr.yml` or auto-detection) that implements the `Tool` trait.
/// Wraps a `ToolEntry` and delegates execution to `execute_shell_command()`.
pub struct ConfiguredTool {
    entry: ToolEntry,
}

impl ConfiguredTool {
    pub fn new(entry: ToolEntry) -> Self {
        Self { entry }
    }
}

#[async_trait]
impl Tool for ConfiguredTool {
    fn name(&self) -> &str {
        &self.entry.name
    }

    fn description(&self) -> &str {
        &self.entry.command
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Additional arguments to pass to the command"
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let args: Vec<String> = input
            .get("args")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let full_command = if args.is_empty() {
            self.entry.command.clone()
        } else {
            format!("{} {}", self.entry.command, args.join(" "))
        };

        match execute_shell_command(&full_command, &ctx.working_dir, self.entry.timeout_secs).await {
            Ok(output) => ToolResult {
                content: format_shell_output(&output),
                is_error: output.exit_code != 0,
            },
            Err(e) => ToolResult {
                content: format!("command failed: {}", e),
                is_error: true,
            },
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn echo_entry() -> ToolEntry {
        ToolEntry {
            name: "echo-test".to_string(),
            command: "echo hello".to_string(),
            timeout_secs: 10,
            worktree: true,
        }
    }

    #[test]
    fn test_configured_tool_name() {
        let tool = ConfiguredTool::new(echo_entry());
        assert_eq!(tool.name(), "echo-test");
    }

    #[test]
    fn test_configured_tool_description() {
        let tool = ConfiguredTool::new(echo_entry());
        assert_eq!(tool.description(), "echo hello");
    }

    #[test]
    fn test_configured_tool_schema() {
        let tool = ConfiguredTool::new(echo_entry());
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["args"].is_object());
    }

    #[tokio::test]
    async fn test_configured_tool_execute_echo() {
        let tool = ConfiguredTool::new(echo_entry());
        let ctx = ToolContext::new(std::env::temp_dir(), "test-1".to_string());
        let result = tool.execute(json!({}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn test_configured_tool_execute_with_args() {
        let tool = ConfiguredTool::new(ToolEntry {
            name: "echo-args".to_string(),
            command: "echo".to_string(),
            timeout_secs: 10,
            worktree: true,
        });
        let ctx = ToolContext::new(std::env::temp_dir(), "test-1".to_string());
        let result = tool.execute(json!({"args": ["foo", "bar"]}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("foo bar"));
    }

    #[tokio::test]
    async fn test_configured_tool_execute_failure() {
        let tool = ConfiguredTool::new(ToolEntry {
            name: "fail".to_string(),
            command: "exit 1".to_string(),
            timeout_secs: 10,
            worktree: true,
        });
        let ctx = ToolContext::new(std::env::temp_dir(), "test-1".to_string());
        let result = tool.execute(json!({}), &ctx).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn test_configured_tool_execute_timeout() {
        let tool = ConfiguredTool::new(ToolEntry {
            name: "timeout".to_string(),
            command: "sleep 60".to_string(),
            timeout_secs: 1,
            worktree: true,
        });
        let ctx = ToolContext::new(std::env::temp_dir(), "test-1".to_string());
        let result = tool.execute(json!({}), &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("timed out"));
    }
}
