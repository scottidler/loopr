use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::lane::classify;
use crate::tools::shell::{execute_in_lane, format_shell_output};
use crate::tools::traits::{Tool, ToolResult};

pub struct FindTool;

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files by name pattern"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Name pattern to search for (e.g., '*.rs', 'Cargo.toml')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: '.')"
                },
                "type": {
                    "type": "string",
                    "description": "File type: 'f' for files, 'd' for directories"
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
        let file_type = input.get("type").and_then(|v| v.as_str());

        let mut cmd = format!("find '{}' -name '{}'", path, pattern.replace('\'', "'\\''"));
        if let Some(ft) = file_type {
            cmd = format!("{} -type {}", cmd, ft);
        }
        cmd = format!("{} | head -100", cmd);

        let lane = classify("find");
        match execute_in_lane(&cmd, &ctx.working_dir, lane, 30, &ctx.router).await {
            Ok(output) => ToolResult {
                content: format_shell_output(&output),
                is_error: output.exit_code != 0,
            },
            Err(e) => ToolResult {
                content: format!("find failed: {}", e),
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
    async fn test_find_tool() {
        let dir = std::env::temp_dir().join("loopr-find-test");
        let _ = std::fs::create_dir_all(dir.join("sub"));
        std::fs::write(dir.join("sub/main.rs"), "").unwrap();

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = FindTool.execute(json!({"pattern": "*.rs"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("main.rs"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
