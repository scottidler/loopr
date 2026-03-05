use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Exact string replacement in a file"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact string to find and replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement string"
                }
            },
            "required": ["path", "old_string", "new_string"]
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
        let old_string = match input.get("old_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return ToolResult {
                    content: "missing required parameter: old_string".into(),
                    is_error: true,
                };
            }
        };
        let new_string = match input.get("new_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return ToolResult {
                    content: "missing required parameter: new_string".into(),
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

        let count = content.matches(old_string).count();
        if count == 0 {
            return ToolResult {
                content: format!("old_string not found in '{}'", path),
                is_error: true,
            };
        }
        if count > 1 {
            return ToolResult {
                content: format!("old_string found {} times in '{}' — must be unique", count, path),
                is_error: true,
            };
        }

        let new_content = content.replacen(old_string, new_string, 1);
        match tokio::fs::write(&full_path, &new_content).await {
            Ok(()) => ToolResult {
                content: format!("edited {}", path),
                is_error: false,
            },
            Err(e) => ToolResult {
                content: format!("failed to write '{}': {}", path, e),
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
    async fn test_edit_tool() {
        let dir = std::env::temp_dir().join("loopr-edit-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("test.rs"), "fn hello() {}").unwrap();

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = EditTool
            .execute(
                json!({"path": "test.rs", "old_string": "hello", "new_string": "world"}),
                &ctx,
            )
            .await;
        assert!(!result.is_error);

        let content = std::fs::read_to_string(dir.join("test.rs")).unwrap();
        assert_eq!(content, "fn world() {}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_tool_not_found() {
        let dir = std::env::temp_dir().join("loopr-edit-nf");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("test.rs"), "fn hello() {}").unwrap();

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = EditTool
            .execute(
                json!({"path": "test.rs", "old_string": "xyz", "new_string": "abc"}),
                &ctx,
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
