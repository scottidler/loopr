//! Tool routing and execution
//!
//! Defines the ToolRouter trait for executing tools and LocalToolRouter for in-process execution.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::{LooprError, Result};
use crate::llm::{ToolCall, ToolResult};

use super::catalog::ToolCatalog;
use super::definition::ToolLane;

/// Trait for routing and executing tool calls
#[async_trait]
pub trait ToolRouter: Send + Sync {
    /// Execute a tool call in the given worktree context
    async fn execute(&self, call: ToolCall, worktree: &Path) -> Result<ToolResult>;

    /// Get list of available tool names
    fn available_tools(&self) -> Vec<String>;
}

/// Local in-process tool router for development and testing
pub struct LocalToolRouter {
    catalog: ToolCatalog,
    max_output_bytes: usize,
}

impl LocalToolRouter {
    /// Create a new local tool router with the given catalog
    pub fn new(catalog: ToolCatalog) -> Self {
        Self {
            catalog,
            max_output_bytes: 100_000,
        }
    }

    /// Set maximum output size in bytes
    pub fn with_max_output(mut self, max_bytes: usize) -> Self {
        self.max_output_bytes = max_bytes;
        self
    }

    /// Execute a bash command in the given directory
    async fn execute_bash(&self, command: &str, cwd: &Path, timeout_ms: u64) -> Result<(String, bool)> {
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| LooprError::Tool(format!("Failed to spawn bash: {}", e)))?;

        let timeout = Duration::from_millis(timeout_ms);

        let result = tokio::time::timeout(timeout, async {
            let status = child.wait().await?;
            let mut stdout = String::new();
            let mut stderr = String::new();

            if let Some(mut out) = child.stdout.take() {
                out.read_to_string(&mut stdout).await?;
            }
            if let Some(mut err) = child.stderr.take() {
                err.read_to_string(&mut stderr).await?;
            }

            Ok::<_, std::io::Error>((status, stdout, stderr))
        })
        .await;

        match result {
            Ok(Ok((status, stdout, stderr))) => {
                let mut output = stdout;
                if !stderr.is_empty() {
                    if !output.is_empty() {
                        output.push_str("\n--- stderr ---\n");
                    }
                    output.push_str(&stderr);
                }

                // Truncate if too long
                if output.len() > self.max_output_bytes {
                    output.truncate(self.max_output_bytes);
                    output.push_str("\n... [output truncated]");
                }

                Ok((output, status.success()))
            }
            Ok(Err(e)) => Err(LooprError::Tool(format!("IO error: {}", e))),
            Err(_) => {
                // Timeout - try to kill the process
                let _ = child.kill().await;
                Err(LooprError::Tool(format!(
                    "Command timed out after {}ms",
                    timeout_ms
                )))
            }
        }
    }

    /// Read a file with optional offset and limit
    async fn execute_read_file(
        &self,
        path: &Path,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<String> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| LooprError::Tool(format!("Failed to read file: {}", e)))?;

        let lines: Vec<&str> = content.lines().collect();
        let offset = offset.unwrap_or(0);
        let limit = limit.unwrap_or(2000);

        let selected: Vec<String> = lines
            .iter()
            .skip(offset)
            .take(limit)
            .enumerate()
            .map(|(i, line)| format!("{:>6}  {}", offset + i + 1, line))
            .collect();

        let mut output = selected.join("\n");
        if output.len() > self.max_output_bytes {
            output.truncate(self.max_output_bytes);
            output.push_str("\n... [output truncated]");
        }

        Ok(output)
    }

    /// Write content to a file
    async fn execute_write_file(&self, path: &Path, content: &str) -> Result<String> {
        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| LooprError::Tool(format!("Failed to create directories: {}", e)))?;
        }

        tokio::fs::write(path, content)
            .await
            .map_err(|e| LooprError::Tool(format!("Failed to write file: {}", e)))?;

        Ok(format!("Successfully wrote {} bytes to {}", content.len(), path.display()))
    }
}

#[async_trait]
impl ToolRouter for LocalToolRouter {
    async fn execute(&self, call: ToolCall, worktree: &Path) -> Result<ToolResult> {
        let tool = self.catalog.get(&call.name).ok_or_else(|| {
            LooprError::Tool(format!("Unknown tool: {}", call.name))
        })?;

        // Check if tool requires worktree and worktree exists
        if tool.requires_worktree && !worktree.exists() {
            return Ok(ToolResult::error(
                call.id,
                format!("Worktree does not exist: {}", worktree.display()),
            ));
        }

        let timeout_ms = tool.effective_timeout_ms();

        let result = match call.name.as_str() {
            "bash" | "run_command" => {
                let command = call.input["command"]
                    .as_str()
                    .ok_or_else(|| LooprError::Tool("Missing 'command' parameter".into()))?;

                match self.execute_bash(command, worktree, timeout_ms).await {
                    Ok((output, success)) => {
                        if success {
                            ToolResult::success(call.id, output)
                        } else {
                            ToolResult::error(call.id, output)
                        }
                    }
                    Err(e) => ToolResult::error(call.id, e.to_string()),
                }
            }

            "read_file" => {
                let path_str = call.input["path"]
                    .as_str()
                    .ok_or_else(|| LooprError::Tool("Missing 'path' parameter".into()))?;
                let path = worktree.join(path_str);
                let offset = call.input["offset"].as_u64().map(|v| v as usize);
                let limit = call.input["limit"].as_u64().map(|v| v as usize);

                match self.execute_read_file(&path, offset, limit).await {
                    Ok(content) => ToolResult::success(call.id, content),
                    Err(e) => ToolResult::error(call.id, e.to_string()),
                }
            }

            "write_file" => {
                let path_str = call.input["path"]
                    .as_str()
                    .ok_or_else(|| LooprError::Tool("Missing 'path' parameter".into()))?;
                let content = call.input["content"]
                    .as_str()
                    .ok_or_else(|| LooprError::Tool("Missing 'content' parameter".into()))?;
                let path = worktree.join(path_str);

                match self.execute_write_file(&path, content).await {
                    Ok(msg) => ToolResult::success(call.id, msg),
                    Err(e) => ToolResult::error(call.id, e.to_string()),
                }
            }

            _ => {
                // For unknown tools, try to execute as bash if it's in no-net lane
                if tool.lane == ToolLane::NoNet {
                    ToolResult::error(
                        call.id,
                        format!("Tool '{}' is not implemented in LocalToolRouter", call.name),
                    )
                } else {
                    ToolResult::error(
                        call.id,
                        format!(
                            "Tool '{}' requires lane {:?} which is not supported locally",
                            call.name, tool.lane
                        ),
                    )
                }
            }
        };

        Ok(result)
    }

    fn available_tools(&self) -> Vec<String> {
        self.catalog.list().into_iter().map(String::from).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    /// Mock tool router for testing
    #[derive(Default)]
    pub struct MockToolRouter {
        responses: std::collections::HashMap<String, String>,
        tools: Vec<String>,
    }

    impl MockToolRouter {
        /// Create a new mock router with predefined responses
        pub fn new() -> Self {
            Self::default()
        }

        /// Add a predefined response for a tool
        pub fn with_response(mut self, tool_name: &str, response: &str) -> Self {
            self.responses
                .insert(tool_name.to_string(), response.to_string());
            self
        }

        /// Add available tools
        pub fn with_tools(mut self, tools: Vec<String>) -> Self {
            self.tools = tools;
            self
        }
    }

    #[async_trait]
    impl ToolRouter for MockToolRouter {
        async fn execute(&self, call: ToolCall, _worktree: &Path) -> Result<ToolResult> {
            if let Some(response) = self.responses.get(&call.name) {
                Ok(ToolResult::success(call.id, response.clone()))
            } else {
                Ok(ToolResult::error(
                    call.id,
                    format!("No mock response configured for tool: {}", call.name),
                ))
            }
        }

        fn available_tools(&self) -> Vec<String> {
            self.tools.clone()
        }
    }

    fn create_test_catalog() -> ToolCatalog {
        let mut catalog = ToolCatalog::new();
        catalog.add(super::super::Tool::new("bash", "Execute bash command").with_worktree_required());
        catalog.add(super::super::Tool::new("read_file", "Read file contents").with_worktree_required());
        catalog.add(super::super::Tool::new("write_file", "Write file contents").with_worktree_required());
        catalog
    }

    #[test]
    fn test_local_router_new() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        assert_eq!(router.max_output_bytes, 100_000);
    }

    #[test]
    fn test_local_router_with_max_output() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog).with_max_output(50_000);
        assert_eq!(router.max_output_bytes, 50_000);
    }

    #[test]
    fn test_local_router_available_tools() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let tools = router.available_tools();
        assert!(tools.contains(&"bash".to_string()));
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"write_file".to_string()));
    }

    #[tokio::test]
    async fn test_execute_bash_echo() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new("call_1", "bash", json!({"command": "echo hello"}));
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn test_execute_bash_failure() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new("call_1", "bash", json!({"command": "exit 1"}));
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(result.is_error);
    }

    #[tokio::test]
    async fn test_execute_bash_missing_command() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new("call_1", "bash", json!({}));
        let result = router.execute(call, temp_dir.path()).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_read_file() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        // Create test file
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "line 1\nline 2\nline 3").unwrap();

        let call = ToolCall::new("call_1", "read_file", json!({"path": "test.txt"}));
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("line 1"));
        assert!(result.content.contains("line 2"));
        assert!(result.content.contains("line 3"));
    }

    #[tokio::test]
    async fn test_execute_read_file_with_offset() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        // Create test file
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "line 1\nline 2\nline 3").unwrap();

        let call = ToolCall::new("call_1", "read_file", json!({"path": "test.txt", "offset": 1, "limit": 1}));
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("line 2"));
        assert!(!result.content.contains("line 1"));
    }

    #[tokio::test]
    async fn test_execute_read_file_not_found() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new("call_1", "read_file", json!({"path": "nonexistent.txt"}));
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Failed to read file"));
    }

    #[tokio::test]
    async fn test_execute_write_file() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new(
            "call_1",
            "write_file",
            json!({"path": "output.txt", "content": "test content"}),
        );
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Successfully wrote"));

        // Verify file was written
        let written = std::fs::read_to_string(temp_dir.path().join("output.txt")).unwrap();
        assert_eq!(written, "test content");
    }

    #[tokio::test]
    async fn test_execute_write_file_creates_dirs() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new(
            "call_1",
            "write_file",
            json!({"path": "nested/dir/output.txt", "content": "nested content"}),
        );
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(!result.is_error);

        // Verify file was written
        let written = std::fs::read_to_string(temp_dir.path().join("nested/dir/output.txt")).unwrap();
        assert_eq!(written, "nested content");
    }

    #[tokio::test]
    async fn test_execute_unknown_tool() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new("call_1", "unknown_tool", json!({}));
        let result = router.execute(call, temp_dir.path()).await;

        assert!(result.is_err());
    }

    #[test]
    fn test_mock_router_new() {
        let router = MockToolRouter::new();
        assert!(router.responses.is_empty());
        assert!(router.tools.is_empty());
    }

    #[test]
    fn test_mock_router_with_response() {
        let router = MockToolRouter::new().with_response("test_tool", "test output");
        assert!(router.responses.contains_key("test_tool"));
    }

    #[test]
    fn test_mock_router_with_tools() {
        let router = MockToolRouter::new().with_tools(vec!["tool1".to_string(), "tool2".to_string()]);
        assert_eq!(router.available_tools(), vec!["tool1", "tool2"]);
    }

    #[tokio::test]
    async fn test_mock_router_execute_configured() {
        let router = MockToolRouter::new().with_response("my_tool", "configured response");
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new("call_1", "my_tool", json!({}));
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(!result.is_error);
        assert_eq!(result.content, "configured response");
    }

    #[tokio::test]
    async fn test_mock_router_execute_unconfigured() {
        let router = MockToolRouter::new();
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new("call_1", "unconfigured_tool", json!({}));
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("No mock response configured"));
    }

    #[tokio::test]
    async fn test_execute_bash_with_stderr() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        let call = ToolCall::new("call_1", "bash", json!({"command": "echo stdout && echo stderr >&2"}));
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("stdout"));
        assert!(result.content.contains("stderr"));
    }

    #[tokio::test]
    async fn test_read_file_line_numbers() {
        let catalog = create_test_catalog();
        let router = LocalToolRouter::new(catalog);
        let temp_dir = TempDir::new().unwrap();

        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "first\nsecond\nthird").unwrap();

        let call = ToolCall::new("call_1", "read_file", json!({"path": "test.txt"}));
        let result = router.execute(call, temp_dir.path()).await.unwrap();

        // Should have line numbers
        assert!(result.content.contains("1"));
        assert!(result.content.contains("2"));
        assert!(result.content.contains("3"));
    }
}
