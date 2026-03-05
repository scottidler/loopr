use async_trait::async_trait;
use serde_json::json;

use crate::tools::context::ToolContext;
use crate::tools::traits::{Tool, ToolResult};

/// Plan tool — allows the LLM to create or update execution plans. Agent-only tool.
pub struct PlanTool;

#[async_trait]
impl Tool for PlanTool {
    fn name(&self) -> &str {
        "plan"
    }

    fn description(&self) -> &str {
        "Create or update an execution plan"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "view"],
                    "description": "Action to perform on the plan"
                },
                "content": {
                    "type": "string",
                    "description": "Plan content (for 'create' or 'update')"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let action = match input.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return ToolResult {
                    content: "missing required parameter: action".into(),
                    is_error: true,
                };
            }
        };

        // Placeholder — plan management integrated with the agentic loop
        ToolResult {
            content: format!("plan action '{}' acknowledged (not yet implemented)", action),
            is_error: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_plan_tool() {
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "test".into());
        let result = PlanTool.execute(json!({"action": "view"}), &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("view"));
    }
}
