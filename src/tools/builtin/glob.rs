use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern (e.g., '**/*.rs', 'src/**/*.ts')"
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

        let full_pattern = ctx.working_dir.join(pattern).to_string_lossy().to_string();

        let matches: Vec<String> = match glob::glob(&full_pattern) {
            Ok(paths) => paths
                .filter_map(|p| p.ok())
                .map(|p| {
                    p.strip_prefix(&ctx.working_dir)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .to_string()
                })
                .collect(),
            Err(e) => {
                return ToolResult {
                    content: format!("invalid glob pattern: {}", e),
                    is_error: true,
                };
            }
        };

        ToolResult {
            content: if matches.is_empty() {
                "no matches found".to_string()
            } else {
                matches.join("\n")
            },
            is_error: false,
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_glob_tool() {
        let dir = std::env::temp_dir().join("loopr-glob-test");
        let _ = std::fs::create_dir_all(dir.join("src"));
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = GlobTool.execute(json!({"pattern": "src/*.rs"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("main.rs"));
        assert!(result.content.contains("lib.rs"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_glob_tool_no_matches() {
        let dir = std::env::temp_dir().join("loopr-glob-empty");
        let _ = std::fs::create_dir_all(&dir);

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = GlobTool.execute(json!({"pattern": "*.xyz"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("no matches"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
