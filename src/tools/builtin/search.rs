use std::future::Future;
use std::pin::Pin;

use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

/// Web search tool — placeholder for future integration.
pub struct SearchTool;

impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "Search the web for information"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                }
            },
            "required": ["query"]
        })
    }

    fn execute<'a>(
        &'a self,
        input: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let query = match input.get("query").and_then(|v| v.as_str()) {
                Some(q) => q,
                None => {
                    return ToolResult {
                        content: "missing required parameter: query".into(),
                        is_error: true,
                    };
                }
            };

            // Placeholder - web search requires an external API
            ToolResult {
                content: format!("search for '{}' acknowledged (not yet implemented)", query),
                is_error: false,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_search_tool() {
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "test".into());
        let result = SearchTool.execute(json!({"query": "rust async"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("rust async"));
    }
}
