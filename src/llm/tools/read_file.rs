//! read_file tool - Read file contents with line numbers

use async_trait::async_trait;
use eyre::eyre;
use serde_json::Value;
use std::path::Path;

use super::{Tool, ToolContext, ToolResult};

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a file's contents with line numbers. Required before editing."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to worktree"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max lines to read (default: 2000)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, eyre::Error> {
        let path = input["path"].as_str().ok_or_else(|| eyre!("path is required"))?;
        let offset = input["offset"].as_u64().unwrap_or(1) as usize;
        let limit = input["limit"].as_u64().unwrap_or(2000) as usize;

        let full_path = ctx.validate_path(Path::new(path))?;

        let content = tokio::fs::read_to_string(&full_path)
            .await
            .map_err(|e| eyre!("Failed to read file '{}': {}", path, e))?;

        // Track read for edit validation
        ctx.track_read(&full_path).await;

        // Format with line numbers (cat -n style)
        let lines: Vec<_> = content
            .lines()
            .skip(offset.saturating_sub(1))
            .take(limit)
            .enumerate()
            .map(|(i, line)| {
                let line_num = offset + i;
                let truncated = if line.len() > 2000 {
                    format!("{}...", &line[..2000])
                } else {
                    line.to_string()
                };
                format!("{:>6}|{}", line_num, truncated)
            })
            .collect();

        Ok(ToolResult::success(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_read_file_basic() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "line 1\nline 2\nline 3").unwrap();

        let tool = ReadFileTool;
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("line 1"));
        assert!(result.content.contains("line 2"));
        assert!(result.content.contains("line 3"));
    }

    #[tokio::test]
    async fn test_read_file_with_offset() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "line 1\nline 2\nline 3\nline 4\nline 5").unwrap();

        let tool = ReadFileTool;
        let result = tool
            .execute(serde_json::json!({"path": "test.txt", "offset": 3}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(!result.content.contains("line 1"));
        assert!(!result.content.contains("line 2"));
        assert!(result.content.contains("line 3"));
        assert!(result.content.contains("line 4"));
    }

    #[tokio::test]
    async fn test_read_file_with_limit() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "line 1\nline 2\nline 3\nline 4\nline 5").unwrap();

        let tool = ReadFileTool;
        let result = tool
            .execute(serde_json::json!({"path": "test.txt", "limit": 2}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("line 1"));
        assert!(result.content.contains("line 2"));
        assert!(!result.content.contains("line 3"));
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = ReadFileTool;
        let result = tool.execute(serde_json::json!({"path": "nonexistent.txt"}), &ctx).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_file_tracks_read() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "content").unwrap();

        assert!(!ctx.was_read(&test_file).await);

        let tool = ReadFileTool;
        let _ = tool
            .execute(serde_json::json!({"path": "test.txt"}), &ctx)
            .await
            .unwrap();

        assert!(ctx.was_read(&test_file).await);
    }

    #[tokio::test]
    async fn test_read_file_line_numbers() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "a\nb\nc").unwrap();

        let tool = ReadFileTool;
        let result = tool
            .execute(serde_json::json!({"path": "test.txt"}), &ctx)
            .await
            .unwrap();

        // Should have line numbers prefixed
        assert!(result.content.contains("1|a"));
        assert!(result.content.contains("2|b"));
        assert!(result.content.contains("3|c"));
    }
}
