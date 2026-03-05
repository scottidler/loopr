use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::shell::{execute_shell_command, format_shell_output};
use crate::tools::traits::{Tool, ToolResult};

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with regex"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in (default: '.')"
                },
                "include": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g., '*.rs')"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let pattern = match input.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult {
                    content: "missing required parameter: pattern".into(),
                    is_error: true,
                };
            }
        };
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let include = input.get("include").and_then(|v| v.as_str());

        let mut cmd = format!("grep -rn '{}' '{}'", pattern.replace('\'', "'\\''"), path);
        if let Some(inc) = include {
            cmd = format!("{} --include='{}'", cmd, inc);
        }
        // Limit output
        cmd = format!("{} | head -100", cmd);

        match execute_shell_command(&cmd, &ctx.working_dir, 30).await {
            Ok(output) => ToolResult {
                content: format_shell_output(&output),
                // grep returns exit 1 for no matches — that's not an error
                is_error: output.exit_code > 1,
            },
            Err(e) => ToolResult {
                content: format!("grep failed: {}", e),
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
    async fn test_grep_tool() {
        let dir = std::env::temp_dir().join("loopr-grep-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("test.rs"), "fn hello_world() {}\nfn goodbye() {}").unwrap();

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = GrepTool.execute(json!({"pattern": "hello"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("hello_world"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
