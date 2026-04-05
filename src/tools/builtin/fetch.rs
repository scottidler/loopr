use std::future::Future;
use std::pin::Pin;

use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

/// Fetch a URL and return its content.
pub struct FetchTool;

impl Tool for FetchTool {
    fn name(&self) -> &str {
        "fetch"
    }

    fn description(&self) -> &str {
        "Fetch content from a URL"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to fetch"
                }
            },
            "required": ["url"]
        })
    }

    fn execute<'a>(
        &'a self,
        input: serde_json::Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let url = match input.get("url").and_then(|v| v.as_str()) {
                Some(u) => u,
                None => {
                    return ToolResult {
                        content: "missing required parameter: url".into(),
                        is_error: true,
                    };
                }
            };

            match reqwest::get(url).await {
                Ok(resp) => {
                    let status = resp.status();
                    match resp.text().await {
                        Ok(body) => {
                            let truncated = if body.len() > 32000 {
                                format!("{}...\n(truncated, {} bytes total)", &body[..32000], body.len())
                            } else {
                                body
                            };
                            ToolResult {
                                content: format!("HTTP {}\n\n{}", status, truncated),
                                is_error: !status.is_success(),
                            }
                        }
                        Err(e) => ToolResult {
                            content: format!("failed to read response body: {}", e),
                            is_error: true,
                        },
                    }
                }
                Err(e) => ToolResult {
                    content: format!("fetch failed: {}", e),
                    is_error: true,
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_fetch_tool_missing_url() {
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "test".into());
        let result = FetchTool.execute(json!({}), &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("missing"));
    }
}
