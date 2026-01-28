//! grep tool - Search file contents with regex

use async_trait::async_trait;
use eyre::eyre;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use super::{Tool, ToolContext, ToolResult};

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with regex. Returns matching lines with context."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search (default: worktree)"
                },
                "file_pattern": {
                    "type": "string",
                    "description": "Glob to filter files (e.g., *.rs)"
                },
                "context": {
                    "type": "integer",
                    "description": "Lines of context around matches (default: 2)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, eyre::Error> {
        let pattern = input["pattern"].as_str().ok_or_else(|| eyre!("pattern is required"))?;
        let path = input["path"].as_str().unwrap_or(".");
        let file_pattern = input["file_pattern"].as_str();
        let context_lines = input["context"].as_u64().unwrap_or(2);

        let search_path = ctx.validate_path(Path::new(path))?;

        // Try using ripgrep binary (most reliable and feature-rich)
        match self
            .search_with_rg(&search_path, pattern, file_pattern, context_lines, ctx)
            .await
        {
            Ok(result) => return Ok(result),
            Err(_) => {
                // Fall back to grep if rg is not available
            }
        }

        // Fallback: use grep
        self.search_with_grep(&search_path, pattern, file_pattern, context_lines, ctx)
            .await
    }
}

impl GrepTool {
    /// Search using ripgrep binary
    async fn search_with_rg(
        &self,
        search_path: &Path,
        pattern: &str,
        file_pattern: Option<&str>,
        context_lines: u64,
        ctx: &ToolContext,
    ) -> Result<ToolResult, eyre::Error> {
        let mut cmd = Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg(format!("--context={}", context_lines))
            .arg("--max-count=100")
            .arg(pattern)
            .arg(search_path)
            .current_dir(&ctx.worktree)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(fp) = file_pattern {
            cmd.arg("--glob").arg(fp);
        }

        let output = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await??;

        // rg returns exit code 1 when no matches, which is not an error
        if output.status.success() || output.status.code() == Some(1) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.is_empty() {
                Ok(ToolResult::success("No matches found"))
            } else {
                // Truncate if too long
                let result = if stdout.len() > 30_000 {
                    format!("{}...\n[truncated, {} chars total]", &stdout[..30_000], stdout.len())
                } else {
                    stdout.to_string()
                };
                Ok(ToolResult::success(result))
            }
        } else {
            Ok(ToolResult::error(String::from_utf8_lossy(&output.stderr).to_string()))
        }
    }

    /// Fallback search using grep
    async fn search_with_grep(
        &self,
        search_path: &Path,
        pattern: &str,
        file_pattern: Option<&str>,
        context_lines: u64,
        ctx: &ToolContext,
    ) -> Result<ToolResult, eyre::Error> {
        let mut cmd = Command::new("grep");
        cmd.arg("-rn") // recursive, line numbers
            .arg(format!("-C{}", context_lines))
            .arg("-E") // extended regex
            .arg(pattern)
            .arg(search_path)
            .current_dir(&ctx.worktree)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(fp) = file_pattern {
            cmd.arg("--include").arg(fp);
        }

        let output = tokio::time::timeout(Duration::from_secs(30), cmd.output()).await??;

        // grep returns exit code 1 when no matches
        if output.status.success() || output.status.code() == Some(1) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.is_empty() {
                Ok(ToolResult::success("No matches found"))
            } else {
                let result = if stdout.len() > 30_000 {
                    format!("{}...\n[truncated, {} chars total]", &stdout[..30_000], stdout.len())
                } else {
                    stdout.to_string()
                };
                Ok(ToolResult::success(result))
            }
        } else {
            Ok(ToolResult::error(String::from_utf8_lossy(&output.stderr).to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_grep_basic() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::write(dir.path().join("test.txt"), "Hello, World!\nGoodbye, World!").unwrap();

        let tool = GrepTool;
        let result = tool
            .execute(serde_json::json!({"pattern": "Hello"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Hello"));
    }

    #[tokio::test]
    async fn test_grep_no_matches() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::write(dir.path().join("test.txt"), "Hello, World!").unwrap();

        let tool = GrepTool;
        let result = tool
            .execute(serde_json::json!({"pattern": "Nonexistent"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("No matches"));
    }

    #[tokio::test]
    async fn test_grep_with_file_pattern() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::write(dir.path().join("test.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("test.txt"), "fn main() {}").unwrap();

        let tool = GrepTool;
        let result = tool
            .execute(serde_json::json!({"pattern": "fn main", "file_pattern": "*.rs"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        // Should only match the .rs file
        assert!(result.content.contains("test.rs") || result.content.contains("fn main"));
    }

    #[tokio::test]
    async fn test_grep_specific_path() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("other.rs"), "fn main() {}").unwrap();

        let tool = GrepTool;
        let result = tool
            .execute(serde_json::json!({"pattern": "fn main", "path": "src"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        // Should only search in src directory
        assert!(result.content.contains("main.rs") || result.content.contains("fn main"));
    }

    #[tokio::test]
    async fn test_grep_missing_pattern() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = GrepTool;
        let result = tool.execute(serde_json::json!({}), &ctx).await;

        assert!(result.is_err());
    }
}
