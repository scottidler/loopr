use log::debug;

use crate::tools::context::ToolContext;
use crate::tools::executor::ToolExecutor;
use crate::tools::traits::ToolResult;
use crate::tools::types::{ContentBlock, Message, ToolCall, ToolDefinition};

/// Conversation state for the agentic tool loop.
pub struct AgenticConversation {
    pub messages: Vec<Message>,
    pub system_prompt: String,
    pub tool_definitions: Vec<ToolDefinition>,
}

/// Result of running the agentic loop.
pub struct AgenticResult {
    /// Final text output from the LLM (concatenated from all text blocks in the final turn).
    pub text: String,
    /// All messages exchanged during the loop.
    pub messages: Vec<Message>,
    /// Total number of tool calls made.
    pub tool_calls_count: usize,
}

/// Callback trait for the LLM completion step in the agentic loop.
/// Separated from the existing LlmClient to avoid coupling.
#[async_trait::async_trait]
pub trait AgenticLlm: Send + Sync {
    async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> eyre::Result<(Vec<ContentBlock>, Option<crate::tools::types::StopReason>)>;
}

/// Run the agentic tool loop:
/// 1. Call LLM with tools
/// 2. If tool_use blocks returned, execute them
/// 3. Feed tool results back as user message
/// 4. Repeat until end_turn or max iterations
pub async fn run_tool_loop(
    llm: &dyn AgenticLlm,
    executor: &ToolExecutor,
    ctx: &ToolContext,
    system_prompt: &str,
    initial_message: &str,
    max_iterations: u32,
) -> eyre::Result<AgenticResult> {
    debug!(
        "run_tool_loop(max_iterations={}, tools={})",
        max_iterations,
        executor.available_tools().len()
    );

    let tool_defs = executor.definitions();
    let mut messages = vec![Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: initial_message.to_string(),
        }],
    }];

    let mut total_tool_calls = 0;

    for iteration in 0..max_iterations {
        debug!("agentic_loop iteration {}", iteration);

        let (content_blocks, stop_reason) = llm.complete(system_prompt, &messages, &tool_defs).await?;

        // Add assistant message
        messages.push(Message {
            role: "assistant".to_string(),
            content: content_blocks.clone(),
        });

        // Check for tool_use blocks
        let tool_uses: Vec<ToolCall> = content_blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                _ => None,
            })
            .collect();

        if tool_uses.is_empty() || stop_reason != Some(crate::tools::types::StopReason::ToolUse) {
            // No tool calls or end_turn — extract final text and return
            let text = extract_text(&content_blocks);
            return Ok(AgenticResult {
                text,
                messages,
                tool_calls_count: total_tool_calls,
            });
        }

        // Execute all tool calls
        let mut tool_results = Vec::new();
        for call in &tool_uses {
            debug!("executing tool: {} (id: {})", call.name, call.id);
            let result: ToolResult = executor.execute(call, ctx).await;
            total_tool_calls += 1;
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: result.content,
                is_error: result.is_error,
            });
        }

        // Add tool results as user message
        messages.push(Message {
            role: "user".to_string(),
            content: tool_results,
        });
    }

    // Max iterations reached — return what we have
    let last_text = messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map(|m| extract_text(&m.content))
        .unwrap_or_default();

    Ok(AgenticResult {
        text: last_text,
        messages,
        tool_calls_count: total_tool_calls,
    })
}

/// Extract text content from content blocks.
fn extract_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock LLM that returns a text response, then optionally tool calls.
    struct MockAgenticLlm {
        responses: Vec<(Vec<ContentBlock>, Option<crate::tools::types::StopReason>)>,
        call_count: AtomicUsize,
    }

    impl MockAgenticLlm {
        fn new(responses: Vec<(Vec<ContentBlock>, Option<crate::tools::types::StopReason>)>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl AgenticLlm for MockAgenticLlm {
        async fn complete(
            &self,
            _system_prompt: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> eyre::Result<(Vec<ContentBlock>, Option<crate::tools::types::StopReason>)> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            if idx < self.responses.len() {
                Ok(self.responses[idx].clone())
            } else {
                Ok((
                    vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    Some(crate::tools::types::StopReason::EndTurn),
                ))
            }
        }
    }

    #[tokio::test]
    async fn test_run_tool_loop_no_tools() {
        let llm = MockAgenticLlm::new(vec![(
            vec![ContentBlock::Text {
                text: "Hello!".to_string(),
            }],
            Some(crate::tools::types::StopReason::EndTurn),
        )]);
        let executor = ToolExecutor::standard(&[]);
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "test".into());

        let result = run_tool_loop(&llm, &executor, &ctx, "system", "hi", 10).await.unwrap();
        assert_eq!(result.text, "Hello!");
        assert_eq!(result.tool_calls_count, 0);
        assert_eq!(result.messages.len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn test_run_tool_loop_with_tool_call() {
        let dir = std::env::temp_dir().join("loopr-agentic-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("test.txt"), "hello world").unwrap();

        let llm = MockAgenticLlm::new(vec![
            // First response: tool use
            (
                vec![ContentBlock::ToolUse {
                    id: "tu-1".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::json!({"command": "echo tool-executed"}),
                }],
                Some(crate::tools::types::StopReason::ToolUse),
            ),
            // Second response: final text
            (
                vec![ContentBlock::Text {
                    text: "Done executing tool.".to_string(),
                }],
                Some(crate::tools::types::StopReason::EndTurn),
            ),
        ]);
        let executor = ToolExecutor::standard(&[]);
        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);

        let result = run_tool_loop(&llm, &executor, &ctx, "system", "run echo", 10)
            .await
            .unwrap();
        assert_eq!(result.text, "Done executing tool.");
        assert_eq!(result.tool_calls_count, 1);
        // user + assistant(tool_use) + user(tool_result) + assistant(text)
        assert_eq!(result.messages.len(), 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_run_tool_loop_max_iterations() {
        // LLM always returns tool_use — should stop at max_iterations
        let llm = MockAgenticLlm::new(
            (0..20)
                .map(|i| {
                    (
                        vec![ContentBlock::ToolUse {
                            id: format!("tu-{}", i),
                            name: "shell".to_string(),
                            input: serde_json::json!({"command": "echo iteration"}),
                        }],
                        Some(crate::tools::types::StopReason::ToolUse),
                    )
                })
                .collect(),
        );
        let executor = ToolExecutor::standard(&[]);
        let ctx = ToolContext::new(std::env::temp_dir(), "test".into());

        let result = run_tool_loop(&llm, &executor, &ctx, "system", "loop", 3).await.unwrap();
        assert_eq!(result.tool_calls_count, 3);
    }

    #[test]
    fn test_extract_text() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello ".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tu-1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({}),
            },
            ContentBlock::Text {
                text: "world".to_string(),
            },
        ];
        assert_eq!(extract_text(&blocks), "hello world");
    }
}
