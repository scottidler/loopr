//! list_directory tool - List files and directories in a path

use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

use super::{Tool, ToolContext, ToolResult};

pub struct ListDirectoryTool;

#[async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &'static str {
        "list_directory"
    }

    fn description(&self) -> &'static str {
        "List files and directories in a path."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path relative to worktree (default: .)"
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, eyre::Error> {
        let path = input["path"].as_str().unwrap_or(".");
        let full_path = ctx.validate_path(Path::new(path))?;

        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(&full_path).await?;

        while let Some(entry) = dir.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry.metadata().await?;

            let suffix = if metadata.is_dir() { "/" } else { "" };
            entries.push(format!("{}{}", name, suffix));
        }

        entries.sort();

        if entries.is_empty() {
            Ok(ToolResult::success("(empty directory)"))
        } else {
            Ok(ToolResult::success(entries.join("\n")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_list_directory_basic() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        // Create some files and directories
        std::fs::write(dir.path().join("file1.txt"), "content").unwrap();
        std::fs::write(dir.path().join("file2.txt"), "content").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let tool = ListDirectoryTool;
        let result = tool.execute(serde_json::json!({"path": "."}), &ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("file1.txt"));
        assert!(result.content.contains("file2.txt"));
        assert!(result.content.contains("subdir/"));
    }

    #[tokio::test]
    async fn test_list_directory_default_path() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::write(dir.path().join("test.txt"), "content").unwrap();

        let tool = ListDirectoryTool;
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("test.txt"));
    }

    #[tokio::test]
    async fn test_list_directory_empty() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = ListDirectoryTool;
        let result = tool.execute(serde_json::json!({"path": "."}), &ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("empty"));
    }

    #[tokio::test]
    async fn test_list_directory_sorted() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::write(dir.path().join("zebra.txt"), "").unwrap();
        std::fs::write(dir.path().join("apple.txt"), "").unwrap();
        std::fs::write(dir.path().join("banana.txt"), "").unwrap();

        let tool = ListDirectoryTool;
        let result = tool.execute(serde_json::json!({"path": "."}), &ctx).await.unwrap();

        let lines: Vec<_> = result.content.lines().collect();
        assert!(lines[0].contains("apple"));
        assert!(lines[1].contains("banana"));
        assert!(lines[2].contains("zebra"));
    }

    #[tokio::test]
    async fn test_list_subdirectory() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("subdir/nested.txt"), "").unwrap();

        let tool = ListDirectoryTool;
        let result = tool.execute(serde_json::json!({"path": "subdir"}), &ctx).await.unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("nested.txt"));
    }

    #[tokio::test]
    async fn test_list_nonexistent_directory() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = ListDirectoryTool;
        let result = tool.execute(serde_json::json!({"path": "nonexistent"}), &ctx).await;

        assert!(result.is_err());
    }
}
