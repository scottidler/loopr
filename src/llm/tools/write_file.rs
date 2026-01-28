//! write_file tool - Write content to a file

use async_trait::async_trait;
use eyre::eyre;
use serde_json::Value;
use std::path::Path;

use super::{Tool, ToolContext, ToolResult};

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Write content to a file. Creates parent directories if needed."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to worktree"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, eyre::Error> {
        let path = input["path"].as_str().ok_or_else(|| eyre!("path is required"))?;
        let content = input["content"].as_str().ok_or_else(|| eyre!("content is required"))?;

        let full_path = ctx.validate_path(Path::new(path))?;

        // Create parent directories
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&full_path, content).await?;

        Ok(ToolResult::success(format!(
            "Wrote {} bytes to {}",
            content.len(),
            path
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_write_file_basic() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = WriteFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "test.txt",
                    "content": "Hello, World!"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("13 bytes"));

        // Verify file was written
        let content = std::fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "Hello, World!");
    }

    #[tokio::test]
    async fn test_write_file_creates_directories() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = WriteFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "subdir/nested/test.txt",
                    "content": "Nested content"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);

        // Verify file was written in nested directory
        let content = std::fs::read_to_string(dir.path().join("subdir/nested/test.txt")).unwrap();
        assert_eq!(content, "Nested content");
    }

    #[tokio::test]
    async fn test_write_file_overwrites_existing() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        // Create initial file
        std::fs::write(dir.path().join("test.txt"), "Old content").unwrap();

        let tool = WriteFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "test.txt",
                    "content": "New content"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);

        // Verify file was overwritten
        let content = std::fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(content, "New content");
    }

    #[tokio::test]
    async fn test_write_file_missing_path() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = WriteFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "content": "Hello"
                }),
                &ctx,
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_write_file_missing_content() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = WriteFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "test.txt"
                }),
                &ctx,
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_write_file_empty_content() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = WriteFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "empty.txt",
                    "content": ""
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("0 bytes"));

        // Verify empty file was created
        let content = std::fs::read_to_string(dir.path().join("empty.txt")).unwrap();
        assert!(content.is_empty());
    }
}
