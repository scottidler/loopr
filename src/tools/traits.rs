use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Result of executing a tool — used by all tools (built-in and configured).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

/// A tool that can be executed by the ToolExecutor.
/// Both built-in tools (read, write, grep) and configured tools (test, lint, build)
/// implement this trait.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    async fn execute(&self, input: serde_json::Value, ctx: &super::context::ToolContext) -> ToolResult;
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_result_serde_roundtrip() {
        let result = ToolResult {
            content: "hello".to_string(),
            is_error: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.content, "hello");
        assert!(!deserialized.is_error);
    }

    #[test]
    fn test_tool_result_error() {
        let result = ToolResult {
            content: "failed".to_string(),
            is_error: true,
        };
        assert!(result.is_error);
        assert_eq!(result.content, "failed");
    }
}
