use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

pub struct TreeTool;

#[async_trait]
impl Tool for TreeTool {
    fn name(&self) -> &str {
        "tree"
    }

    fn description(&self) -> &str {
        "Show directory tree structure"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Root directory (relative to working directory, default: '.')"
                },
                "depth": {
                    "type": "integer",
                    "description": "Maximum depth to traverse (default: 3)"
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let max_depth = input.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;

        let full_path = match ctx.validate_path(path) {
            Ok(p) => p,
            Err(e) => {
                return ToolResult {
                    content: e.to_string(),
                    is_error: true,
                };
            }
        };

        let mut output = Vec::new();
        build_tree(&full_path, "", 0, max_depth, &mut output);

        ToolResult {
            content: output.join("\n"),
            is_error: false,
        }
    }
}

fn build_tree(path: &std::path::Path, prefix: &str, depth: usize, max_depth: usize, output: &mut Vec<String>) {
    if depth > max_depth {
        return;
    }

    let mut entries: Vec<_> = match std::fs::read_dir(path) {
        Ok(iter) => iter.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());

    for (i, entry) in entries.iter().enumerate() {
        let is_last = i == entries.len() - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let name = entry.file_name().to_string_lossy().to_string();

        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            output.push(format!("{}{}{}/", prefix, connector, name));
            let next_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
            build_tree(&entry.path(), &next_prefix, depth + 1, max_depth, output);
        } else {
            output.push(format!("{}{}{}", prefix, connector, name));
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tree_tool() {
        let dir = std::env::temp_dir().join("loopr-tree-test");
        let _ = std::fs::create_dir_all(dir.join("src"));
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();

        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);
        let result = TreeTool.execute(json!({}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("src/"));
        assert!(result.content.contains("main.rs"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
