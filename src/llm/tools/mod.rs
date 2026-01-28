//! Tool system for LLM interactions
//!
//! Tools provide file system access, command execution, and coordination capabilities
//! to Ralph loops. Each loop gets a ToolContext scoped to its git worktree.

#![allow(dead_code)]

mod complete_task;
mod context;
mod edit_file;
mod executor;
mod glob_tool;
mod grep;
mod list_directory;
mod read_file;
mod run_command;
mod write_file;

pub use context::{ToolContext, ToolError};
pub use executor::ToolExecutor;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

/// A tool that can be called by the LLM
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (matches LLM tool_use name)
    fn name(&self) -> &'static str;

    /// Human-readable description
    fn description(&self) -> &'static str;

    /// JSON Schema for input parameters
    fn input_schema(&self) -> Value;

    /// Execute the tool
    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, eyre::Error>;
}

/// Result from tool execution
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Tool definition for the LLM API
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDefinition {
    /// Convert to Anthropic API schema format
    pub fn to_anthropic_schema(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
        })
    }
}

// Re-export individual tools for direct access if needed
pub use complete_task::CompleteTaskTool;
pub use edit_file::EditFileTool;
pub use glob_tool::GlobTool;
pub use grep::GrepTool;
pub use list_directory::ListDirectoryTool;
pub use read_file::ReadFileTool;
pub use run_command::RunCommandTool;
pub use write_file::WriteFileTool;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_result_success() {
        let result = ToolResult::success("Operation completed");
        assert_eq!(result.content, "Operation completed");
        assert!(!result.is_error);
    }

    #[test]
    fn test_tool_result_error() {
        let result = ToolResult::error("Something went wrong");
        assert_eq!(result.content, "Something went wrong");
        assert!(result.is_error);
    }

    #[test]
    fn test_tool_definition_to_anthropic() {
        let def = ToolDefinition {
            name: "test_tool".to_string(),
            description: "A test tool".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "input": {"type": "string"}
                }
            }),
        };

        let schema = def.to_anthropic_schema();
        assert_eq!(schema["name"], "test_tool");
        assert_eq!(schema["description"], "A test tool");
    }
}
