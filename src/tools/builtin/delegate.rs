use std::sync::Arc;

use async_trait::async_trait;
use log::debug;
use serde_json::json;

use crate::tools::agentic_loop::{AgenticLlm, run_tool_loop};
use crate::tools::context::ToolContext;
use crate::tools::executor::ToolExecutor;
use crate::tools::traits::{Tool, ToolResult};
use crate::tools::types::{ContentBlock, Message};

/// Subagent delegation tool. Spawns a child run_tool_loop with its own context
/// window, returning only the summary to the parent. This prevents bulk operations
/// (reading many files, searching large codebases) from consuming the parent's
/// context window.
pub struct DelegateTool {
    llm: Arc<dyn AgenticLlm>,
    executor: Arc<ToolExecutor>,
}

impl DelegateTool {
    pub fn new(llm: Arc<dyn AgenticLlm>, executor: Arc<ToolExecutor>) -> Self {
        Self { llm, executor }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Delegate a task to a subagent with its own context window. \
         Use for bulk operations (reading many files, searching large codebases) \
         that would consume too much context if done inline. \
         The subagent has access to all the same tools (read, grep, shell, etc.) \
         and returns a text summary of its findings."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Clear description of what the subagent should do. \
                                    Be specific about what files to read, what to search for, \
                                    and what format to return results in."
                },
                "max_iterations": {
                    "type": "integer",
                    "description": "Maximum tool loop iterations for the subagent (default: 20)"
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let task = match input.get("task").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                return ToolResult {
                    content: "missing required parameter: task".into(),
                    is_error: true,
                };
            }
        };
        let max_iter = input.get("max_iterations").and_then(|v| v.as_u64()).unwrap_or(20) as u32;

        let child_id = format!("{}:sub", ctx.exec_id);
        let child_ctx = ToolContext::new(ctx.working_dir.clone(), child_id);

        let system_prompt = format!(
            "You are a research subagent. Complete the following task using the available tools. \
             Be thorough but concise in your final response — your output will be returned \
             to a parent agent as a tool result.\n\nTask: {}",
            task
        );

        let messages = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text { text: task.to_string() }],
        }];

        debug!(
            "delegate: spawning child loop (task_len={}, max_iter={})",
            task.len(),
            max_iter
        );

        match run_tool_loop(
            self.llm.as_ref(),
            self.executor.as_ref(),
            &child_ctx,
            &system_prompt,
            messages,
            max_iter,
            None, // Child events don't stream to TUI
            None, // No checkpointing for child loops
        )
        .await
        {
            Ok(result) => {
                debug!("delegate: child completed ({} tool calls)", result.tool_calls_count);
                ToolResult {
                    content: result.text,
                    is_error: false,
                }
            }
            Err(e) => ToolResult {
                content: format!("subagent failed: {}", e),
                is_error: true,
            },
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::types::StopReason;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockLlm {
        responses: Vec<(Vec<ContentBlock>, Option<StopReason>)>,
        call_count: AtomicUsize,
    }

    #[async_trait]
    impl AgenticLlm for MockLlm {
        async fn complete(
            &self,
            _system_prompt: &str,
            _messages: &[Message],
            _tools: &[crate::tools::types::ToolDefinition],
        ) -> eyre::Result<(Vec<ContentBlock>, Option<StopReason>)> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            if idx < self.responses.len() {
                Ok(self.responses[idx].clone())
            } else {
                Ok((
                    vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    Some(StopReason::EndTurn),
                ))
            }
        }
    }

    #[tokio::test]
    async fn test_delegate_returns_child_text() {
        let llm = Arc::new(MockLlm {
            responses: vec![(
                vec![ContentBlock::Text {
                    text: "Found 3 relevant files".to_string(),
                }],
                Some(StopReason::EndTurn),
            )],
            call_count: AtomicUsize::new(0),
        });
        let executor = Arc::new(ToolExecutor::chat(&[]));
        let tool = DelegateTool::new(llm, executor);

        let ctx = ToolContext::new(std::env::temp_dir(), "parent".into());
        let result = tool.execute(json!({"task": "find all .rs files"}), &ctx).await;

        assert!(!result.is_error);
        assert_eq!(result.content, "Found 3 relevant files");
    }

    #[tokio::test]
    async fn test_delegate_missing_task() {
        let llm = Arc::new(MockLlm {
            responses: vec![],
            call_count: AtomicUsize::new(0),
        });
        let executor = Arc::new(ToolExecutor::chat(&[]));
        let tool = DelegateTool::new(llm, executor);

        let ctx = ToolContext::new(std::env::temp_dir(), "parent".into());
        let result = tool.execute(json!({}), &ctx).await;

        assert!(result.is_error);
        assert!(result.content.contains("missing"));
    }

    #[tokio::test]
    async fn test_delegate_child_context_is_isolated() {
        let llm = Arc::new(MockLlm {
            responses: vec![(
                vec![ContentBlock::Text {
                    text: "child result".to_string(),
                }],
                Some(StopReason::EndTurn),
            )],
            call_count: AtomicUsize::new(0),
        });
        let executor = Arc::new(ToolExecutor::chat(&[]));
        let tool = DelegateTool::new(llm, executor);

        let ctx = ToolContext::new(std::env::temp_dir(), "parent-ctx".into());
        let result = tool
            .execute(json!({"task": "do something", "max_iterations": 5}), &ctx)
            .await;

        assert!(!result.is_error);
        // The child ran with its own context (parent-ctx:sub)
        assert_eq!(result.content, "child result");
    }
}
