//! run_command tool - Execute shell commands in the worktree

use async_trait::async_trait;
use eyre::eyre;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use super::{Tool, ToolContext, ToolResult};

pub struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command in the worktree. Use for git, build tools, tests."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 120000)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, eyre::Error> {
        let command = input["command"].as_str().ok_or_else(|| eyre!("command is required"))?;
        let timeout_ms = input["timeout_ms"].as_u64().unwrap_or(120_000);

        let output = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&ctx.worktree)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| eyre!("Command timed out after {}ms", timeout_ms))??;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let result = if stdout.is_empty() && !stderr.is_empty() {
            stderr.to_string()
        } else if stderr.is_empty() {
            stdout.to_string()
        } else {
            format!("{}\n\nSTDERR:\n{}", stdout, stderr)
        };

        // Truncate long output
        let truncated = if result.len() > 30_000 {
            format!("{}...\n[truncated, {} chars total]", &result[..30_000], result.len())
        } else {
            result
        };

        if output.status.success() {
            Ok(ToolResult::success(truncated))
        } else {
            Ok(ToolResult::error(format!(
                "Exit code: {}\n{}",
                output.status.code().unwrap_or(-1),
                truncated
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_run_command_echo() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = RunCommandTool;
        let result = tool
            .execute(serde_json::json!({"command": "echo 'Hello, World!'"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Hello, World!"));
    }

    #[tokio::test]
    async fn test_run_command_in_worktree() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        // Create a file to verify we're in the right directory
        std::fs::write(dir.path().join("marker.txt"), "found").unwrap();

        let tool = RunCommandTool;
        let result = tool
            .execute(serde_json::json!({"command": "cat marker.txt"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("found"));
    }

    #[tokio::test]
    async fn test_run_command_failure() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = RunCommandTool;
        let result = tool
            .execute(serde_json::json!({"command": "exit 1"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Exit code: 1"));
    }

    #[tokio::test]
    async fn test_run_command_stderr() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = RunCommandTool;
        let result = tool
            .execute(serde_json::json!({"command": "echo 'error message' >&2"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("error message"));
    }

    #[tokio::test]
    async fn test_run_command_timeout() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = RunCommandTool;
        let result = tool
            .execute(serde_json::json!({"command": "sleep 10", "timeout_ms": 100}), &ctx)
            .await;

        // Should timeout
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("timed out"));
    }

    #[tokio::test]
    async fn test_run_command_missing_command() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = RunCommandTool;
        let result = tool.execute(serde_json::json!({}), &ctx).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_command_ls() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::write(dir.path().join("file1.txt"), "").unwrap();
        std::fs::write(dir.path().join("file2.txt"), "").unwrap();

        let tool = RunCommandTool;
        let result = tool.execute(serde_json::json!({"command": "ls"}), &ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("file1.txt"));
        assert!(result.content.contains("file2.txt"));
    }
}
