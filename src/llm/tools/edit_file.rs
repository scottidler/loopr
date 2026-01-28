//! edit_file tool - Replace a specific string in a file

use async_trait::async_trait;
use eyre::eyre;
use serde_json::Value;
use std::path::Path;

use super::{Tool, ToolContext, ToolResult};

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace a specific string in a file. Requires prior read_file call."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to worktree"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact string to find and replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement string"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences (default: false)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, eyre::Error> {
        let path = input["path"].as_str().ok_or_else(|| eyre!("path is required"))?;
        let old_string = input["old_string"]
            .as_str()
            .ok_or_else(|| eyre!("old_string is required"))?;
        let new_string = input["new_string"]
            .as_str()
            .ok_or_else(|| eyre!("new_string is required"))?;
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);

        let full_path = ctx.validate_path(Path::new(path))?;

        // Must read file first
        if !ctx.was_read(&full_path).await {
            return Ok(ToolResult::error(
                "Must read_file before editing. Read the file first to see current content.",
            ));
        }

        let content = tokio::fs::read_to_string(&full_path).await?;

        // Verify old_string exists
        if !content.contains(old_string) {
            return Ok(ToolResult::error(
                "old_string not found in file. Make sure it matches exactly including whitespace.",
            ));
        }

        // Verify uniqueness (unless replace_all)
        if !replace_all {
            let count = content.matches(old_string).count();
            if count > 1 {
                return Ok(ToolResult::error(format!(
                    "old_string found {} times. Use replace_all=true or provide more context.",
                    count
                )));
            }
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        tokio::fs::write(&full_path, &new_content).await?;

        let replacements = if replace_all { content.matches(old_string).count() } else { 1 };

        Ok(ToolResult::success(format!(
            "Replaced {} occurrence(s) in {}",
            replacements, path
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn setup_context_with_read_file(dir: &tempfile::TempDir) -> ToolContext {
        ToolContext::new(dir.path().to_path_buf(), "test".to_string())
    }

    #[tokio::test]
    async fn test_edit_file_basic() {
        let dir = tempdir().unwrap();
        let ctx = setup_context_with_read_file(&dir).await;

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "Hello, World!").unwrap();

        // Must read first
        ctx.track_read(&test_file).await;

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "test.txt",
                    "old_string": "World",
                    "new_string": "Rust"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("1 occurrence"));

        // Verify file was modified
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "Hello, Rust!");
    }

    #[tokio::test]
    async fn test_edit_file_without_read() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "Hello, World!").unwrap();

        // Don't read first - should fail
        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "test.txt",
                    "old_string": "World",
                    "new_string": "Rust"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Must read_file"));
    }

    #[tokio::test]
    async fn test_edit_file_string_not_found() {
        let dir = tempdir().unwrap();
        let ctx = setup_context_with_read_file(&dir).await;

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "Hello, World!").unwrap();
        ctx.track_read(&test_file).await;

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "test.txt",
                    "old_string": "Nonexistent",
                    "new_string": "New"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

    #[tokio::test]
    async fn test_edit_file_multiple_occurrences_error() {
        let dir = tempdir().unwrap();
        let ctx = setup_context_with_read_file(&dir).await;

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "foo bar foo baz foo").unwrap();
        ctx.track_read(&test_file).await;

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "test.txt",
                    "old_string": "foo",
                    "new_string": "qux"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("3 times"));
    }

    #[tokio::test]
    async fn test_edit_file_replace_all() {
        let dir = tempdir().unwrap();
        let ctx = setup_context_with_read_file(&dir).await;

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "foo bar foo baz foo").unwrap();
        ctx.track_read(&test_file).await;

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "test.txt",
                    "old_string": "foo",
                    "new_string": "qux",
                    "replace_all": true
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("3 occurrence"));

        // Verify all were replaced
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "qux bar qux baz qux");
    }

    #[tokio::test]
    async fn test_edit_file_preserves_whitespace() {
        let dir = tempdir().unwrap();
        let ctx = setup_context_with_read_file(&dir).await;

        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "  indented\n  content").unwrap();
        ctx.track_read(&test_file).await;

        let tool = EditFileTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": "test.txt",
                    "old_string": "  indented",
                    "new_string": "    more indented"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);

        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "    more indented\n  content");
    }
}
