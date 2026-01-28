//! Tool executor - manages tool registration and execution

use std::collections::HashMap;

use super::{
    CompleteTaskTool, EditFileTool, GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, RunCommandTool, Tool,
    ToolContext, ToolDefinition, ToolResult, WriteFileTool,
};
use crate::llm::ToolCall;

/// Manages tool execution for a loop
pub struct ToolExecutor {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolExecutor {
    /// Create executor with standard tools
    pub fn standard() -> Self {
        let mut tools: HashMap<String, Box<dyn Tool>> = HashMap::new();

        // File system tools
        tools.insert("read_file".into(), Box::new(ReadFileTool));
        tools.insert("write_file".into(), Box::new(WriteFileTool));
        tools.insert("edit_file".into(), Box::new(EditFileTool));
        tools.insert("list_directory".into(), Box::new(ListDirectoryTool));
        tools.insert("glob".into(), Box::new(GlobTool));

        // Search
        tools.insert("grep".into(), Box::new(GrepTool));

        // Command execution
        tools.insert("run_command".into(), Box::new(RunCommandTool));

        // Completion signal
        tools.insert("complete_task".into(), Box::new(CompleteTaskTool));

        Self { tools }
    }

    /// Create an empty executor (for custom tool sets)
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }

    /// Add a tool to the executor
    pub fn add_tool(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Get tool definitions for LLM
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Get tool definitions for specific tool names
    pub fn definitions_for(&self, tool_names: &[&str]) -> Vec<ToolDefinition> {
        tool_names
            .iter()
            .filter_map(|name| self.tools.get(*name))
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Execute a tool call
    pub async fn execute(&self, tool_call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        match self.tools.get(&tool_call.name) {
            Some(tool) => match tool.execute(tool_call.input.clone(), ctx).await {
                Ok(result) => result,
                Err(e) => ToolResult::error(format!("Tool error: {}", e)),
            },
            None => ToolResult::error(format!("Unknown tool: {}", tool_call.name)),
        }
    }

    /// Execute multiple tool calls
    pub async fn execute_all(&self, tool_calls: &[ToolCall], ctx: &ToolContext) -> Vec<(String, ToolResult)> {
        let mut results = Vec::with_capacity(tool_calls.len());

        for call in tool_calls {
            let result = self.execute(call, ctx).await;
            results.push((call.id.clone(), result));
        }

        results
    }

    /// Check if a tool exists
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Get the list of tool names
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self::standard()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_standard_executor_has_all_tools() {
        let executor = ToolExecutor::standard();

        assert!(executor.has_tool("read_file"));
        assert!(executor.has_tool("write_file"));
        assert!(executor.has_tool("edit_file"));
        assert!(executor.has_tool("list_directory"));
        assert!(executor.has_tool("glob"));
        assert!(executor.has_tool("grep"));
        assert!(executor.has_tool("run_command"));
        assert!(executor.has_tool("complete_task"));
    }

    #[test]
    fn test_definitions() {
        let executor = ToolExecutor::standard();
        let defs = executor.definitions();

        assert!(!defs.is_empty());
        assert!(defs.iter().any(|d| d.name == "read_file"));
    }

    #[test]
    fn test_definitions_for_subset() {
        let executor = ToolExecutor::standard();
        let defs = executor.definitions_for(&["read_file", "write_file"]);

        assert_eq!(defs.len(), 2);
        assert!(defs.iter().any(|d| d.name == "read_file"));
        assert!(defs.iter().any(|d| d.name == "write_file"));
    }

    #[tokio::test]
    async fn test_execute_unknown_tool() {
        let executor = ToolExecutor::standard();
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        let tool_call = ToolCall {
            id: "call_1".to_string(),
            name: "nonexistent_tool".to_string(),
            input: serde_json::json!({}),
        };

        let result = executor.execute(&tool_call, &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn test_execute_all() {
        let executor = ToolExecutor::standard();
        let dir = tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf(), "test".to_string());

        // Create a test file
        std::fs::write(dir.path().join("test.txt"), "Hello").unwrap();

        let tool_calls = vec![
            ToolCall {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "test.txt"}),
            },
            ToolCall {
                id: "call_2".to_string(),
                name: "list_directory".to_string(),
                input: serde_json::json!({"path": "."}),
            },
        ];

        let results = executor.execute_all(&tool_calls, &ctx).await;
        assert_eq!(results.len(), 2);

        let (id1, result1) = &results[0];
        assert_eq!(id1, "call_1");
        assert!(!result1.is_error);

        let (id2, result2) = &results[1];
        assert_eq!(id2, "call_2");
        assert!(!result2.is_error);
    }

    #[test]
    fn test_empty_executor() {
        let executor = ToolExecutor::new();
        assert!(executor.tool_names().is_empty());
        assert!(executor.definitions().is_empty());
    }

    #[test]
    fn test_add_custom_tool() {
        let mut executor = ToolExecutor::new();
        executor.add_tool(Box::new(ReadFileTool));

        assert!(executor.has_tool("read_file"));
        assert!(!executor.has_tool("write_file"));
    }
}
