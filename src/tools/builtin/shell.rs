use std::sync::Arc;

use std::future::Future;
use std::pin::Pin;

use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::lane::classify;
use crate::tools::router::LaneRouter;
use crate::tools::shell::{execute_in_lane, format_shell_output};
use crate::tools::traits::{Tool, ToolResult};

/// Generic shell tool for ad-hoc commands. Higher risk - the LLM controls the full command string.
/// Supports `run_in_background` for long-running commands (builds, tests).
pub struct ShellTool;

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
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Run in background and return immediately with a task ID and output path. Use read/grep on the output path to check results."
                }
            },
            "required": ["command"]
        })
    }

    fn execute<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
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
            let background = input
                .get("run_in_background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let lane = classify("shell");

            if background {
                let router: Arc<LaneRouter> = Arc::clone(&ctx.router);
                let task = router.spawn_background(command, &ctx.working_dir, lane, Some(timeout));
                return ToolResult {
                    content: format!(
                        "background task started: {}\nOutput will be written to: {}\nUse the read or grep tool on this path to check results.",
                        task.task_id,
                        task.output_path.display()
                    ),
                    is_error: false,
                };
            }

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
        })
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

    #[tokio::test]
    async fn test_shell_tool_background() {
        let dir = std::env::temp_dir();
        let ctx = ToolContext::new(dir, "test-bg".into());
        let result = ShellTool
            .execute(json!({"command": "echo bg-test", "run_in_background": true}), &ctx)
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("background task started"));
        assert!(result.content.contains("loopr-bg"));

        // Give the background task a moment to complete
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}
