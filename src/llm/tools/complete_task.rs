//! complete_task tool - Signal that the current task/phase is complete

use async_trait::async_trait;
use eyre::eyre;
use serde_json::Value;

use super::{Tool, ToolContext, ToolResult};

pub struct CompleteTaskTool;

#[async_trait]
impl Tool for CompleteTaskTool {
    fn name(&self) -> &'static str {
        "complete_task"
    }

    fn description(&self) -> &'static str {
        "Signal that the current task/phase is complete. Only use when validation passes."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Brief summary of what was accomplished"
                }
            },
            "required": ["summary"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolResult, eyre::Error> {
        let summary = input["summary"].as_str().ok_or_else(|| eyre!("summary is required"))?;

        // The loop engine checks this flag and exits if validation passes
        Ok(ToolResult::success(format!("Task marked complete: {}", summary)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_complete_task_basic() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = CompleteTaskTool;
        let result = tool
            .execute(
                serde_json::json!({"summary": "Implemented the feature successfully"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Task marked complete"));
        assert!(result.content.contains("Implemented the feature"));
    }

    #[tokio::test]
    async fn test_complete_task_missing_summary() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = CompleteTaskTool;
        let result = tool.execute(serde_json::json!({}), &ctx).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_complete_task_with_details() {
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool = CompleteTaskTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "summary": "Added user authentication module with JWT support"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("JWT"));
    }
}
