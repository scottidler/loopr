use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write or create a file"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative to working directory)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult {
                    content: "missing required parameter: path".into(),
                    is_error: true,
                };
            }
        };
        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult {
                    content: "missing required parameter: content".into(),
                    is_error: true,
                };
            }
        };

        let full_path = match ctx.validate_path(path) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    content: e.to_string(),
                    is_error: true,
                };
            }
        };

        if let Some(parent) = full_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolResult {
                content: format!("failed to create directory: {}", e),
                is_error: true,
            };
        }

        match tokio::fs::write(&full_path, content).await {
            Ok(()) => ToolResult {
                content: format!("wrote {} bytes to {}", content.len(), path),
                is_error: false,
            },
            Err(e) => ToolResult {
                content: format!("failed to write '{}': {}", path, e),
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
    async fn test_write_tool() {
        let dir = std::env::temp_dir().join("loopr-write-test");
        let _ = std::fs::create_dir_all(&dir);

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = WriteTool
            .execute(json!({"path": "out.txt", "content": "hello world"}), &ctx)
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("11 bytes"));

        let written = std::fs::read_to_string(dir.join("out.txt")).unwrap();
        assert_eq!(written, "hello world");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_write_tool_creates_dirs() {
        let dir = std::env::temp_dir().join("loopr-write-dirs");
        let _ = std::fs::create_dir_all(&dir);

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = WriteTool
            .execute(json!({"path": "sub/dir/file.txt", "content": "nested"}), &ctx)
            .await;
        assert!(!result.is_error);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
