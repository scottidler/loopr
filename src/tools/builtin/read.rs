use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file with line numbers"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative to working directory)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-based)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["path"]
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

        let full_path = match ctx.validate_path(path) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    content: e.to_string(),
                    is_error: true,
                };
            }
        };

        let content = match tokio::fs::read_to_string(&full_path).await {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    content: format!("failed to read '{}': {}", path, e),
                    is_error: true,
                };
            }
        };

        ctx.track_read(&full_path).await;

        let lines: Vec<&str> = content.lines().collect();
        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        let limit = input.get("limit").and_then(|v| v.as_u64()).map(|l| l as usize);

        let start = offset.saturating_sub(1);
        let end = match limit {
            Some(l) => (start + l).min(lines.len()),
            None => lines.len(),
        };

        let numbered: Vec<String> = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
            .collect();

        ToolResult {
            content: numbered.join("\n"),
            is_error: false,
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_read_tool() {
        let dir = std::env::temp_dir().join("loopr-read-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("test.txt"), "line1\nline2\nline3\n").unwrap();

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = ReadTool.execute(json!({"path": "test.txt"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("line1"));
        assert!(result.content.contains("line3"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_read_tool_with_offset_and_limit() {
        let dir = std::env::temp_dir().join("loopr-read-offset");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("test.txt"), "a\nb\nc\nd\ne\n").unwrap();

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = ReadTool
            .execute(json!({"path": "test.txt", "offset": 2, "limit": 2}), &ctx)
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("b"));
        assert!(result.content.contains("c"));
        assert!(!result.content.contains("a"));
        assert!(!result.content.contains("d"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_read_tool_missing_path() {
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "test".into());
        let result = ReadTool.execute(json!({}), &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("missing"));
    }
}
