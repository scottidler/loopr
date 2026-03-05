use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use log::{debug, warn};

use crate::config::ToolEntry;
use crate::tools::agentic_loop::AgenticLlm;
use crate::tools::builtin::delegate::DelegateTool;
use crate::tools::configured::ConfiguredTool;
use crate::tools::context::ToolContext;
use crate::tools::detect::detect_project_tools;
use crate::tools::traits::{Tool, ToolResult};
use crate::tools::types::{ToolCall, ToolDefinition};

/// Unified tool executor — single registry for both built-in and configured tools.
pub struct ToolExecutor {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolExecutor {
    /// Create with all built-in tools plus configured project tools.
    /// Configured tools override built-ins with the same name.
    pub fn standard(configured: &[ToolEntry]) -> Self {
        debug!("ToolExecutor::standard(configured_count={})", configured.len());
        let mut tools: HashMap<String, Box<dyn Tool>> = HashMap::new();

        // Insert builtins first
        for builtin in crate::tools::builtin::all_builtins() {
            tools.insert(builtin.name().to_string(), builtin);
        }

        // Configured tools override builtins
        for entry in configured {
            if tools.contains_key(&entry.name) {
                warn!("Configured tool '{}' overrides built-in tool", entry.name);
            }
            tools.insert(entry.name.clone(), Box::new(ConfiguredTool::new(entry.clone())));
        }
        Self { tools }
    }

    /// Subset for TUI chat (all built-ins + configured, minus agent-only tools).
    pub fn chat(configured: &[ToolEntry]) -> Self {
        let mut exec = Self::standard(configured);
        exec.tools.remove("plan"); // agent-only
        exec
    }

    /// Chat executor with delegate tool for subagent delegation.
    /// The child executor (used by delegate) does NOT include the delegate tool itself,
    /// preventing unbounded recursion.
    pub fn chat_with_delegation(configured: &[ToolEntry], llm: Arc<dyn AgenticLlm>) -> Self {
        let mut exec = Self::chat(configured);
        let child_executor = Arc::new(Self::chat(configured));
        let delegate = DelegateTool::new(llm, child_executor);
        exec.tools.insert("delegate".to_string(), Box::new(delegate));
        exec
    }

    /// Auto-detect project type and register appropriate configured tools.
    pub fn detect_or_configured(worktree: &Path, configured: &[ToolEntry]) -> Self {
        let project_tools = detect_project_tools(worktree, configured);
        Self::standard(&project_tools)
    }

    /// Export tool definitions for the Anthropic API.
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

    /// List available tool names.
    pub fn available_tools(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// Execute a tool call.
    pub async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        match self.tools.get(&call.name) {
            Some(tool) => tool.execute(call.input.clone(), ctx).await,
            None => ToolResult {
                content: format!("unknown tool: {}", call.name),
                is_error: true,
            },
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    fn test_entries() -> Vec<ToolEntry> {
        vec![
            ToolEntry {
                name: "echo-test".to_string(),
                command: "echo hello".to_string(),
                timeout_secs: 10,
                worktree: true,
            },
            ToolEntry {
                name: "fail-test".to_string(),
                command: "exit 1".to_string(),
                timeout_secs: 10,
                worktree: true,
            },
        ]
    }

    const BUILTIN_COUNT: usize = 14;

    #[test]
    fn test_executor_standard() {
        let exec = ToolExecutor::standard(&test_entries());
        // 14 builtins + 2 configured
        assert_eq!(exec.tools.len(), BUILTIN_COUNT + 2);
        let tools = exec.available_tools();
        assert!(tools.contains(&"echo-test"));
        assert!(tools.contains(&"fail-test"));
        // Builtins present
        assert!(tools.contains(&"read"));
        assert!(tools.contains(&"write"));
    }

    #[test]
    fn test_executor_empty() {
        let exec = ToolExecutor::standard(&[]);
        // Only builtins
        assert_eq!(exec.available_tools().len(), BUILTIN_COUNT);
    }

    #[test]
    fn test_executor_definitions() {
        let exec = ToolExecutor::standard(&test_entries());
        let defs = exec.definitions();
        assert_eq!(defs.len(), BUILTIN_COUNT + 2);
        assert!(defs.iter().any(|d| d.name == "echo-test"));
        assert!(defs.iter().any(|d| d.name == "fail-test"));
        assert!(defs.iter().any(|d| d.name == "read"));
    }

    #[tokio::test]
    async fn test_executor_execute_known_tool() {
        let exec = ToolExecutor::standard(&test_entries());
        let ctx = ToolContext::new(std::env::temp_dir(), "test-1".to_string());
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "echo-test".to_string(),
            input: serde_json::json!({}),
        };
        let result = exec.execute(&call, &ctx).await;
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn test_executor_execute_unknown_tool() {
        let exec = ToolExecutor::standard(&test_entries());
        let ctx = ToolContext::new(std::env::temp_dir(), "test-1".to_string());
        let call = ToolCall {
            id: "call-2".to_string(),
            name: "nonexistent".to_string(),
            input: serde_json::json!({}),
        };
        let result = exec.execute(&call, &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown tool"));
    }

    #[test]
    fn test_executor_detect_js() {
        let dir = TestDir::new("loopr-exec-js");
        std::fs::write(dir.join("package.json"), "{}").unwrap();

        let exec = ToolExecutor::detect_or_configured(&dir, &[]);
        let tools = exec.available_tools();
        assert!(tools.contains(&"test"));
        assert!(tools.contains(&"lint"));
    }

    #[test]
    fn test_executor_detect_no_markers() {
        let dir = TestDir::new("loopr-exec-none");

        let configured = vec![ToolEntry {
            name: "custom".into(),
            command: "echo custom".into(),
            timeout_secs: 10,
            worktree: true,
        }];

        let exec = ToolExecutor::detect_or_configured(&dir, &configured);
        assert!(exec.available_tools().contains(&"custom"));
    }

    #[test]
    fn test_executor_definitions_format() {
        let entries = vec![ToolEntry {
            name: "test".to_string(),
            command: "cargo test".to_string(),
            timeout_secs: 300,
            worktree: true,
        }];
        let exec = ToolExecutor::standard(&entries);
        let defs = exec.definitions();
        // 14 builtins + 1 configured (test overrides builtin? no, "test" is not a builtin name)
        assert_eq!(defs.len(), BUILTIN_COUNT + 1);
        let def = defs.iter().find(|d| d.name == "test").expect("test tool not found");
        assert_eq!(def.description, "cargo test");
        assert_eq!(def.input_schema["type"], "object");
    }

    #[test]
    fn test_executor_chat_excludes_plan() {
        let exec = ToolExecutor::chat(&[]);
        let tools = exec.available_tools();
        assert!(!tools.contains(&"plan"), "chat executor should not have plan tool");
        assert!(tools.contains(&"read"), "chat executor should have read tool");
    }
}
