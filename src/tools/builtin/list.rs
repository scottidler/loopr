use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

pub struct ListTool;

#[async_trait]
impl Tool for ListTool {
    fn name(&self) -> &str {
        "list"
    }

    fn description(&self) -> &str {
        "List files and directories"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path to list (relative to working directory, default: '.')"
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let full_path = match ctx.validate_path(path) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    content: e.to_string(),
                    is_error: true,
                };
            }
        };

        let mut entries = match tokio::fs::read_dir(&full_path).await {
            Ok(e) => e,
            Err(e) => {
                return ToolResult {
                    content: format!("failed to list '{}': {}", path, e),
                    is_error: true,
                };
            }
        };

        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().await.map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir {
                names.push(format!("{}/", name));
            } else {
                names.push(name);
            }
        }
        names.sort();

        ToolResult {
            content: names.join("\n"),
            is_error: false,
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_tool() {
        let dir = std::env::temp_dir().join("loopr-list-test");
        let _ = std::fs::create_dir_all(dir.join("subdir"));
        std::fs::write(dir.join("file.txt"), "").unwrap();

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = ListTool.execute(json!({}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("file.txt"));
        assert!(result.content.contains("subdir/"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
