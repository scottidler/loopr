//! glob tool - Find files matching a glob pattern

use async_trait::async_trait;
use eyre::eyre;
use serde_json::Value;
use std::path::Path;

use super::{Tool, ToolContext, ToolResult};

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern (e.g., **/*.rs)"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match"
                },
                "path": {
                    "type": "string",
                    "description": "Base directory (default: worktree root)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, eyre::Error> {
        let pattern = input["pattern"].as_str().ok_or_else(|| eyre!("pattern is required"))?;
        let base = input["path"].as_str().unwrap_or(".");

        let base_path = ctx.validate_path(Path::new(base))?;
        let full_pattern = base_path.join(pattern);

        let matches: Vec<_> = glob::glob(full_pattern.to_str().unwrap_or(""))?
            .filter_map(|r| r.ok())
            .filter(|p| {
                // Sandbox check
                p.starts_with(&ctx.worktree)
            })
            .map(|p| {
                p.strip_prefix(&ctx.worktree)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string()
            })
            .take(1000) // Limit results
            .collect();

        if matches.is_empty() {
            Ok(ToolResult::success("No matches found"))
        } else {
            Ok(ToolResult::success(matches.join("\n")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_glob_basic() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::write(dir.path().join("file1.txt"), "").unwrap();
        std::fs::write(dir.path().join("file2.txt"), "").unwrap();
        std::fs::write(dir.path().join("file.rs"), "").unwrap();

        let tool = GlobTool;
        let result = tool
            .execute(serde_json::json!({"pattern": "*.txt"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("file1.txt"));
        assert!(result.content.contains("file2.txt"));
        assert!(!result.content.contains("file.rs"));
    }

    #[tokio::test]
    async fn test_glob_recursive() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::write(dir.path().join("root.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/nested/lib.rs"), "").unwrap();

        let tool = GlobTool;
        let result = tool
            .execute(serde_json::json!({"pattern": "**/*.rs"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("root.rs"));
        assert!(result.content.contains("main.rs"));
        assert!(result.content.contains("lib.rs"));
    }

    #[tokio::test]
    async fn test_glob_with_base_path() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("other.rs"), "").unwrap();

        let tool = GlobTool;
        let result = tool
            .execute(serde_json::json!({"pattern": "*.rs", "path": "src"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("main.rs"));
        assert!(!result.content.contains("other.rs"));
    }

    #[tokio::test]
    async fn test_glob_no_matches() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::write(dir.path().join("file.txt"), "").unwrap();

        let tool = GlobTool;
        let result = tool
            .execute(serde_json::json!({"pattern": "*.rs"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("No matches"));
    }

    #[tokio::test]
    async fn test_glob_missing_pattern() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = GlobTool;
        let result = tool.execute(serde_json::json!({}), &ctx).await;

        assert!(result.is_err());
    }
}
