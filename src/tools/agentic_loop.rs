use std::future::Future;
use std::time::Instant;

use log::debug;
use tokio::sync::{broadcast, mpsc};

use crate::agents::context::estimate_tokens;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::context::ToolContext;
use crate::tools::executor::ToolExecutor;
use crate::tools::traits::ToolResult;
use crate::tools::types::{ContentBlock, Message, ToolCall, ToolDefinition};

/// Maximum characters per tool result before truncation (~8K tokens).
const MAX_TOOL_RESULT_CHARS: usize = 32_768;

/// Trigger compaction when estimated input tokens exceed this threshold.
/// Set at 75% of the 200K context window to leave room for output tokens,
/// tool definitions (~2K tokens), and estimation error margin.
const COMPACTION_THRESHOLD: usize = 150_000;

/// Minimum number of messages to keep uncompacted (most recent).
/// These are the "working set" the LLM needs for its current task.
const PROTECTED_TAIL_MESSAGES: usize = 6;

/// Max times we'll attempt to recover from output token limit per loop invocation.
const MAX_OUTPUT_RECOVERY_ATTEMPTS: u32 = 3;

/// Microcompact: truncate tool results longer than this (chars) outside the protected tail.
const MICROCOMPACT_THRESHOLD: usize = 4096;

/// How many chars to keep as preview when microcompacting.
const MICROCOMPACT_PREVIEW: usize = 2048;

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

/// Cap a tool result at MAX_TOOL_RESULT_CHARS, truncating at the last newline.
fn cap_tool_result(content: String) -> String {
    if content.len() <= MAX_TOOL_RESULT_CHARS {
        return content;
    }
    let truncated = &content[..MAX_TOOL_RESULT_CHARS];
    if let Some(pos) = truncated.rfind('\n') {
        format!(
            "{}\n... [output truncated at ~{} chars]",
            &truncated[..pos],
            MAX_TOOL_RESULT_CHARS
        )
    } else {
        format!("{}\n... [output truncated]", truncated)
    }
}

/// Estimate the total token count of a message array.
pub fn estimate_message_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => estimate_tokens(text),
                    ContentBlock::ToolUse { name, input, .. } => {
                        estimate_tokens(name) + estimate_tokens(&input.to_string())
                    }
                    ContentBlock::ToolResult { content, .. } => estimate_tokens(content),
                })
                .sum::<usize>()
                + 10 // overhead per message (role, structural JSON)
        })
        .sum()
}

/// Callback trait for the LLM completion step in the agentic loop.
/// Separated from the existing LlmClient to avoid coupling.
pub trait AgenticLlm: Send + Sync {
    fn complete<'a>(
        &'a self,
        system_prompt: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
    ) -> impl Future<Output = eyre::Result<(Vec<ContentBlock>, Option<crate::tools::types::StopReason>)>> + Send + 'a;

    /// Streaming variant: sends completed ContentBlocks through the channel as they
    /// arrive from SSE, enabling tool execution to overlap with model generation.
    /// Default implementation falls back to `complete()` and sends all blocks after.
    fn complete_streaming<'a>(
        &'a self,
        system_prompt: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        block_tx: mpsc::UnboundedSender<ContentBlock>,
    ) -> impl Future<Output = eyre::Result<(Vec<ContentBlock>, Option<crate::tools::types::StopReason>)>> + Send + 'a
    {
        async move {
            let (blocks, stop_reason) = self.complete(system_prompt, messages, tools).await?;
            for block in &blocks {
                let _ = block_tx.send(block.clone());
            }
            Ok((blocks, stop_reason))
        }
    }
}

/// Run the agentic tool loop:
/// 1. Call LLM with tools
/// 2. If tool_use blocks returned, execute them
/// 3. Feed tool results back as user message
/// 4. Repeat until end_turn or max iterations
///
/// `messages` is the full conversation history in Anthropic API format.
/// For a fresh conversation, pass a single user message.
/// Optional callback invoked after each completed iteration (LLM response + tool results).
/// Used by Chat sessions for per-iteration checkpointing.
pub type OnCheckpoint = dyn Fn(&[Message]) + Send + Sync;

/// Auto-compact old messages when context exceeds COMPACTION_THRESHOLD.
/// Uses the LLM to summarize old messages before discarding them.
/// Falls back to dumb truncation if summarization fails.
async fn auto_compact<L: AgenticLlm>(llm: &L, system_prompt: &str, messages: &mut Vec<Message>) {
    let total = estimate_tokens(system_prompt) + estimate_message_tokens(messages);
    if total <= COMPACTION_THRESHOLD {
        return;
    }

    let compact_start = Instant::now();
    let tokens_before = total;

    let mut split = messages.len().saturating_sub(PROTECTED_TAIL_MESSAGES);
    if split == 0 {
        return;
    }
    while split > 0 && messages[split].role != "assistant" {
        split -= 1;
    }
    if split == 0 {
        return;
    }

    let summary_input = format_messages_for_summary(&messages[..split]);

    let summary_prompt = "You are a context compactor. Summarize the following conversation \
        history into a concise recap. Preserve: key findings, file contents that were analyzed, \
        decisions made, and any state the assistant needs to continue its work. \
        Be factual and structured. Use bullet points.";

    let summary_messages = vec![Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: format!("Summarize this conversation history:\n\n{}", summary_input),
        }],
    }];

    match llm.complete(summary_prompt, &summary_messages, &[]).await {
        Ok((blocks, _)) => {
            let summary_text = extract_text(&blocks);
            let summary_message = Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: format!(
                        "[Context compacted — summary of {} earlier messages]\n\n{}",
                        split, summary_text
                    ),
                }],
            };

            let protected = messages[split..].to_vec();
            messages.clear();
            messages.push(summary_message);
            messages.extend(protected);

            let tokens_after = estimate_tokens(system_prompt) + estimate_message_tokens(messages);
            debug!(
                "[timing:auto_compact] {}ms tokens_before={} tokens_after={}",
                compact_start.elapsed().as_millis(),
                tokens_before,
                tokens_after
            );
            debug!(
                "auto_compact: compacted {} messages into summary (~{} tokens), {} protected",
                split,
                estimate_tokens(&summary_text),
                messages.len() - 1
            );
        }
        Err(e) => {
            log::warn!("auto_compact: summarization failed ({}), falling back to truncation", e);
            fallback_truncate(messages, split);
            let tokens_after = estimate_tokens(system_prompt) + estimate_message_tokens(messages);
            debug!(
                "[timing:auto_compact] {}ms tokens_before={} tokens_after={} (fallback)",
                compact_start.elapsed().as_millis(),
                tokens_before,
                tokens_after
            );
        }
    }
}

/// Fallback: replace old tool results with placeholders (no LLM needed).
fn fallback_truncate(messages: &mut [Message], compactable_end: usize) {
    for msg in messages[..compactable_end].iter_mut() {
        for block in msg.content.iter_mut() {
            if let ContentBlock::ToolResult { content, .. } = block
                && estimate_tokens(content) > 50
            {
                *content = "[earlier content truncated — re-read if needed]".to_string();
            }
        }
    }
}

/// Format old messages into a text block for the summarizer.
fn format_messages_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        out.push_str(&format!("--- {} ---\n", msg.role));
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    out.push_str(text);
                    out.push('\n');
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    out.push_str(&format!("[tool call: {} args={}]\n", name, input));
                }
                ContentBlock::ToolResult { content, is_error, .. } => {
                    let prefix = if *is_error { "[tool error]" } else { "[tool result]" };
                    let display = if content.len() > 4000 {
                        format!("{}...[{} chars total]", &content[..4000], content.len())
                    } else {
                        content.clone()
                    };
                    out.push_str(&format!("{} {}\n", prefix, display));
                }
            }
        }
    }
    out
}

/// Lightweight per-turn pruning. No LLM call needed.
/// Truncates oversized tool results outside the protected tail.
fn microcompact(messages: &mut [Message], protected_tail: usize) {
    let compactable_end = messages.len().saturating_sub(protected_tail);
    if compactable_end == 0 {
        return;
    }

    for msg in messages[..compactable_end].iter_mut() {
        for block in msg.content.iter_mut() {
            if let ContentBlock::ToolResult { content, .. } = block
                && content.len() > MICROCOMPACT_THRESHOLD
            {
                let preview_end = MICROCOMPACT_PREVIEW.min(content.len());
                let preview = &content[..preview_end];
                let cut = preview.rfind('\n').unwrap_or(preview_end);
                let original_len = content.len();
                *content = format!(
                    "{}\n... [truncated from {} chars — re-read if needed]",
                    &content[..cut],
                    original_len
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tool_loop<L: AgenticLlm>(
    llm: &L,
    executor: &ToolExecutor,
    ctx: &ToolContext,
    system_prompt: &str,
    messages: Vec<Message>,
    max_iterations: u32,
    event_tx: Option<&broadcast::Sender<DaemonEvent>>,
    on_checkpoint: Option<&OnCheckpoint>,
) -> eyre::Result<AgenticResult> {
    debug!(
        "run_tool_loop(max_iterations={}, messages={}, tools={})",
        max_iterations,
        messages.len(),
        executor.available_tools().len()
    );

    let tool_defs = executor.definitions();
    let mut messages = messages;

    let mut total_tool_calls = 0;
    let mut consecutive_failures: u32 = 0;
    let mut output_recovery_count: u32 = 0;
    const MAX_CONSECUTIVE_FAILURES: u32 = 3;

    let loop_start = Instant::now();

    /// Emit iteration timing via log and optional event channel.
    fn emit_iteration_timing(
        exec_id: &str,
        iteration: u32,
        iter_start: &Instant,
        llm_ms: u128,
        tools_ms: u128,
        tool_count: usize,
        event_tx: Option<&broadcast::Sender<DaemonEvent>>,
    ) {
        let iter_ms = iter_start.elapsed().as_millis();
        let detail = format!(
            "total={}ms llm={}ms tools={}ms tool_count={}",
            iter_ms, llm_ms, tools_ms, tool_count
        );
        debug!("[timing:{}] iter {}: {}", exec_id, iteration, detail);
        if let Some(tx) = event_tx {
            let _ = tx.send(DaemonEvent::agent_timing_info(
                exec_id,
                &format!("iter {iteration}"),
                &detail,
            ));
        }
    }

    /// Emit loop_complete timing via log and optional event channel.
    fn emit_loop_timing(
        exec_id: &str,
        loop_start: &Instant,
        iterations: u32,
        total_tool_calls: usize,
        event_tx: Option<&broadcast::Sender<DaemonEvent>>,
    ) {
        let loop_ms = loop_start.elapsed().as_millis();
        let detail = format!(
            "total={}ms iterations={} tool_calls={}",
            loop_ms, iterations, total_tool_calls
        );
        debug!("[timing:{}] loop_complete: {}", exec_id, detail);
        if let Some(tx) = event_tx {
            let _ = tx.send(DaemonEvent::agent_timing_info(exec_id, "loop_complete", &detail));
        }
    }

    for iteration in 0..max_iterations {
        let iter_start = Instant::now();
        debug!("agentic_loop iteration {}", iteration);

        // Log token estimate for observability
        let estimated_tokens = estimate_tokens(system_prompt) + estimate_message_tokens(&messages);
        debug!(
            "[agent:{}] iteration {} token_estimate={}",
            ctx.exec_id, iteration, estimated_tokens
        );

        // Lightweight per-turn pruning (sub-ms, no LLM call)
        microcompact(&mut messages, PROTECTED_TAIL_MESSAGES);

        // Auto-compact old messages if context is too large
        auto_compact(llm, system_prompt, &mut messages).await;

        // Streaming tool execution: start tools as their blocks complete during SSE
        let (block_tx, mut block_rx) = mpsc::unbounded_channel();

        let llm_start = Instant::now();
        let (llm_result, early_tool_results) = tokio::join!(
            llm.complete_streaming(system_prompt, &messages, &tool_defs, block_tx),
            async {
                use futures::stream::{FuturesUnordered, StreamExt};
                let mut tool_futures = FuturesUnordered::new();
                let mut results: Vec<(ToolCall, ToolResult, u64)> = Vec::new();
                let mut receiving = true;

                loop {
                    tokio::select! {
                        block = block_rx.recv(), if receiving => {
                            match block {
                                Some(ContentBlock::ToolUse { id, name, input }) => {
                                    let call = ToolCall { id, name, input };
                                    debug!(
                                        "[agent:{}] streaming tool_start: tool={} id={}",
                                        ctx.exec_id, call.name, call.id
                                    );
                                    if let Some(tx) = event_tx {
                                        let _ = tx.send(DaemonEvent::agent_tool_started(&ctx.exec_id, &call.name));
                                    }
                                    tool_futures.push(async {
                                        let start = Instant::now();
                                        let result: ToolResult = executor.execute(&call, ctx).await;
                                        let duration_ms = start.elapsed().as_millis() as u64;
                                        (call, result, duration_ms)
                                    });
                                }
                                Some(_) => {} // Text blocks — handled via the full response
                                None => { receiving = false; }
                            }
                        }
                        Some(result) = tool_futures.next() => {
                            results.push(result);
                        }
                        else => break,
                    }
                }
                results
            }
        );

        let llm_ms = llm_start.elapsed().as_millis();
        let (content_blocks, stop_reason) = llm_result?;

        // Tap 2: LLM response finalized
        debug!(
            "[agent:{}] llm_response: stop_reason={:?} blocks={} text_len={}",
            ctx.exec_id,
            stop_reason,
            content_blocks.len(),
            extract_text(&content_blocks).len()
        );

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

        // End turn: no tool calls and not a max_tokens recovery scenario
        if tool_uses.is_empty() && stop_reason != Some(crate::tools::types::StopReason::MaxTokens) {
            emit_iteration_timing(&ctx.exec_id, iteration, &iter_start, llm_ms, 0, 0, event_tx);
            emit_loop_timing(&ctx.exec_id, &loop_start, iteration + 1, total_tool_calls, event_tx);
            let text = extract_text(&content_blocks);
            return Ok(AgenticResult {
                text,
                messages,
                tool_calls_count: total_tool_calls,
            });
        }

        // Max tokens recovery: if the model hit the output limit, inject a
        // "resume" message and continue. Consumes an iteration from the budget.
        if stop_reason == Some(crate::tools::types::StopReason::MaxTokens)
            && output_recovery_count < MAX_OUTPUT_RECOVERY_ATTEMPTS
        {
            output_recovery_count += 1;
            debug!(
                "agentic_loop: max_tokens recovery attempt {}/{}",
                output_recovery_count, MAX_OUTPUT_RECOVERY_ATTEMPTS
            );
            messages.push(Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "Output token limit hit. Resume directly — no apology, no recap \
                           of what you were doing. Pick up mid-thought if that is where the \
                           cut happened. Break remaining work into smaller pieces."
                        .to_string(),
                }],
            });
            if let Some(cb) = on_checkpoint {
                cb(&messages);
            }
            continue;
        }

        // No tool calls and no recovery possible — return
        if tool_uses.is_empty() {
            emit_iteration_timing(&ctx.exec_id, iteration, &iter_start, llm_ms, 0, 0, event_tx);
            emit_loop_timing(&ctx.exec_id, &loop_start, iteration + 1, total_tool_calls, event_tx);
            let text = extract_text(&content_blocks);
            return Ok(AgenticResult {
                text,
                messages,
                tool_calls_count: total_tool_calls,
            });
        }

        // Only execute tools if stop_reason is ToolUse
        if stop_reason != Some(crate::tools::types::StopReason::ToolUse) {
            emit_iteration_timing(&ctx.exec_id, iteration, &iter_start, llm_ms, 0, 0, event_tx);
            emit_loop_timing(&ctx.exec_id, &loop_start, iteration + 1, total_tool_calls, event_tx);
            let text = extract_text(&content_blocks);
            return Ok(AgenticResult {
                text,
                messages,
                tool_calls_count: total_tool_calls,
            });
        }

        // Use early results from streaming, or fall back to collecting any that didn't
        // start streaming (shouldn't happen with correct complete_streaming impl)
        let results = early_tool_results;
        let tools_ms = iter_start.elapsed().as_millis().saturating_sub(llm_ms);

        let mut tool_results = Vec::new();
        let mut all_failed = true;
        for (call, result, duration_ms) in results {
            total_tool_calls += 1;
            if !result.is_error {
                all_failed = false;
            }
            let exit_code = if result.is_error { 1 } else { 0 };
            debug!(
                "[agent:{}] tool_result: tool={} is_error={} exit={} duration={}ms content_len={}",
                ctx.exec_id,
                call.name,
                result.is_error,
                exit_code,
                duration_ms,
                result.content.len()
            );
            if let Some(tx) = event_tx {
                let _ = tx.send(DaemonEvent::agent_tool_completed(
                    &ctx.exec_id,
                    &call.name,
                    exit_code,
                    duration_ms,
                ));
            }
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: cap_tool_result(result.content),
                is_error: result.is_error,
            });
        }

        // Track consecutive all-error iterations
        if all_failed {
            consecutive_failures += 1;
        } else {
            consecutive_failures = 0;
        }

        // Add tool results as user message
        messages.push(Message {
            role: "user".to_string(),
            content: tool_results,
        });

        emit_iteration_timing(
            &ctx.exec_id,
            iteration,
            &iter_start,
            llm_ms,
            tools_ms,
            tool_uses.len(),
            event_tx,
        );

        // Checkpoint: persist messages after each completed iteration
        if let Some(cb) = on_checkpoint {
            cb(&messages);
        }

        // Inject a hint if too many consecutive failures
        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
            debug!(
                "agentic_loop: {} consecutive all-error iterations, injecting hint",
                consecutive_failures
            );
            messages.push(Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: format!(
                        "WARNING: The last {} tool call iterations all failed. \
                         Stop retrying the same approach. Either use a different tool, \
                         change your strategy, or respond with what you know so far.",
                        consecutive_failures
                    ),
                }],
            });
            consecutive_failures = 0;
        }
    }

    // Max iterations reached — return what we have
    emit_loop_timing(&ctx.exec_id, &loop_start, max_iterations, total_tool_calls, event_tx);

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

/// Create a `Vec<Message>` with a single user message (convenience for simple calls).
pub fn user_message(text: &str) -> Vec<Message> {
    vec![Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text { text: text.to_string() }],
    }]
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

        let result = run_tool_loop(&llm, &executor, &ctx, "system", user_message("hi"), 10, None, None)
            .await
            .unwrap();
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

        let result = run_tool_loop(
            &llm,
            &executor,
            &ctx,
            "system",
            user_message("run echo"),
            10,
            None,
            None,
        )
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

        let result = run_tool_loop(&llm, &executor, &ctx, "system", user_message("loop"), 3, None, None)
            .await
            .unwrap();
        assert_eq!(result.tool_calls_count, 3);
    }

    #[tokio::test]
    async fn test_run_tool_loop_with_prior_messages() {
        // Simulate a multi-turn conversation: prior user + assistant, then new user message
        let llm = MockAgenticLlm::new(vec![(
            vec![ContentBlock::Text {
                text: "I remember the prior context.".to_string(),
            }],
            Some(crate::tools::types::StopReason::EndTurn),
        )]);
        let executor = ToolExecutor::standard(&[]);
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "test".into());

        let messages = vec![
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "hello".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: "hi there".to_string(),
                }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "do you remember?".to_string(),
                }],
            },
        ];

        let result = run_tool_loop(&llm, &executor, &ctx, "system", messages, 10, None, None)
            .await
            .unwrap();
        assert_eq!(result.text, "I remember the prior context.");
        // 3 prior messages + 1 assistant response
        assert_eq!(result.messages.len(), 4);
        assert_eq!(result.tool_calls_count, 0);
    }

    #[tokio::test]
    async fn test_run_tool_loop_emits_tool_events() {
        let dir = std::env::temp_dir().join("loopr-agentic-events");
        let _ = std::fs::create_dir_all(&dir);

        let llm = MockAgenticLlm::new(vec![
            (
                vec![ContentBlock::ToolUse {
                    id: "tu-1".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::json!({"command": "echo test"}),
                }],
                Some(crate::tools::types::StopReason::ToolUse),
            ),
            (
                vec![ContentBlock::Text {
                    text: "Done.".to_string(),
                }],
                Some(crate::tools::types::StopReason::EndTurn),
            ),
        ]);
        let executor = ToolExecutor::standard(&[]);
        let ctx = ToolContext::new(dir.clone(), "test-session".into()).with_deny_patterns(vec![]);

        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        let result = run_tool_loop(&llm, &executor, &ctx, "system", user_message("go"), 10, Some(&tx), None)
            .await
            .unwrap();
        assert_eq!(result.tool_calls_count, 1);

        // Should have received ToolStarted + ToolCompleted events
        let event1 = rx.try_recv().unwrap();
        assert_eq!(event1.event, "agent.tool_started");

        let event2 = rx.try_recv().unwrap();
        assert_eq!(event2.event, "agent.tool_completed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_run_tool_loop_consecutive_failure_hint() {
        // LLM calls a tool that always errors — after 3 consecutive failures,
        // a hint message should be injected into the conversation.
        let llm = MockAgenticLlm::new(
            (0..10)
                .map(|i| {
                    (
                        vec![ContentBlock::ToolUse {
                            id: format!("tu-{}", i),
                            name: "read".to_string(),
                            input: serde_json::json!({"path": "nonexistent-file.txt"}),
                        }],
                        Some(crate::tools::types::StopReason::ToolUse),
                    )
                })
                .collect(),
        );
        let executor = ToolExecutor::standard(&[]);
        let dir = std::env::temp_dir().join("loopr-fail-hint");
        let _ = std::fs::create_dir_all(&dir);
        let ctx = ToolContext::new(dir.clone(), "test".into()).with_deny_patterns(vec![]);

        let result = run_tool_loop(&llm, &executor, &ctx, "system", user_message("read it"), 6, None, None)
            .await
            .unwrap();

        // After 3 consecutive failures, a hint user message should appear
        let hint_messages: Vec<_> = result
            .messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content.iter().any(|b| match b {
                        ContentBlock::Text { text } => text.contains("WARNING"),
                        _ => false,
                    })
            })
            .collect();
        assert!(!hint_messages.is_empty(), "expected at least one failure hint message");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_user_message_helper() {
        let msgs = user_message("hello");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello"),
            _ => panic!("expected Text block"),
        }
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

    #[test]
    fn test_cap_tool_result_under_limit() {
        let content = "short content".to_string();
        let capped = cap_tool_result(content.clone());
        assert_eq!(capped, content);
    }

    #[test]
    fn test_cap_tool_result_over_limit() {
        let content = "x\n".repeat(20_000); // ~40K chars
        let capped = cap_tool_result(content);
        assert!(capped.len() < MAX_TOOL_RESULT_CHARS + 100);
        assert!(capped.contains("output truncated"));
    }

    #[test]
    fn test_cap_tool_result_no_newline() {
        let content = "x".repeat(40_000);
        let capped = cap_tool_result(content);
        assert!(capped.contains("output truncated"));
    }

    #[test]
    fn test_estimate_message_tokens_empty() {
        assert_eq!(estimate_message_tokens(&[]), 0);
    }

    #[test]
    fn test_estimate_message_tokens_text() {
        let messages = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "hello world".to_string(),
            }],
        }];
        let tokens = estimate_message_tokens(&messages);
        // ~3 tokens + 10 overhead = ~13
        assert!(tokens > 0);
        assert!(tokens < 50);
    }

    #[test]
    fn test_estimate_message_tokens_tool_result() {
        let messages = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "a".repeat(4000),
                is_error: false,
            }],
        }];
        let tokens = estimate_message_tokens(&messages);
        // ~1000 tokens + 10 overhead
        assert!(tokens > 500);
    }

    #[tokio::test]
    async fn test_run_tool_loop_max_tokens_recovery() {
        let llm = MockAgenticLlm::new(vec![
            // First response: text but hit max_tokens
            (
                vec![ContentBlock::Text {
                    text: "Starting analysis of the cod".to_string(),
                }],
                Some(crate::tools::types::StopReason::MaxTokens),
            ),
            // Second response: resumed text, end_turn
            (
                vec![ContentBlock::Text {
                    text: "ebase. Here is the full answer.".to_string(),
                }],
                Some(crate::tools::types::StopReason::EndTurn),
            ),
        ]);
        let executor = ToolExecutor::standard(&[]);
        let ctx = ToolContext::new(PathBuf::from("/tmp"), "test".into());

        let result = run_tool_loop(&llm, &executor, &ctx, "system", user_message("analyze"), 5, None, None)
            .await
            .unwrap();

        // The recovery message should be in the conversation
        let recovery_msgs: Vec<_> = result
            .messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content.iter().any(|b| match b {
                        ContentBlock::Text { text } => text.contains("Output token limit hit"),
                        _ => false,
                    })
            })
            .collect();
        assert_eq!(recovery_msgs.len(), 1, "expected exactly one recovery message");

        // Final text should be from the second response
        assert!(result.text.contains("full answer"));
    }

    #[test]
    fn test_microcompact_truncates_old_results() {
        let mut messages = vec![
            // Old message with oversized tool result
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: format!("line one\n{}", "x".repeat(5000)),
                    is_error: false,
                }],
            },
            // Recent messages (protected tail)
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: "thinking...".to_string(),
                }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "continue".to_string(),
                }],
            },
        ];

        microcompact(&mut messages, 2);

        // The old tool result should be truncated
        if let ContentBlock::ToolResult { content, .. } = &messages[0].content[0] {
            assert!(
                content.len() < 3000,
                "expected truncated content, got {} chars",
                content.len()
            );
            assert!(content.contains("truncated from"));
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn test_microcompact_preserves_protected_tail() {
        let big_content = "y".repeat(5000);
        let mut messages = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: big_content.clone(),
                is_error: false,
            }],
        }];

        // All messages are in the protected tail
        microcompact(&mut messages, 1);

        // Should NOT be truncated
        if let ContentBlock::ToolResult { content, .. } = &messages[0].content[0] {
            assert_eq!(content.len(), 5000, "protected tail should not be truncated");
        } else {
            panic!("expected ToolResult");
        }
    }

    /// Mock LLM that sends blocks through the channel with a delay to simulate
    /// true SSE streaming (tools start before message_stop).
    struct StreamingMockLlm {
        responses: Vec<(Vec<ContentBlock>, Option<crate::tools::types::StopReason>)>,
        call_count: AtomicUsize,
    }

    impl StreamingMockLlm {
        fn new(responses: Vec<(Vec<ContentBlock>, Option<crate::tools::types::StopReason>)>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
            }
        }
    }

    impl AgenticLlm for StreamingMockLlm {
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

        async fn complete_streaming(
            &self,
            system_prompt: &str,
            messages: &[Message],
            tools: &[ToolDefinition],
            block_tx: mpsc::UnboundedSender<ContentBlock>,
        ) -> eyre::Result<(Vec<ContentBlock>, Option<crate::tools::types::StopReason>)> {
            let (blocks, stop_reason) = self.complete(system_prompt, messages, tools).await?;
            // Send blocks one at a time with a small yield to simulate SSE streaming
            for block in &blocks {
                let _ = block_tx.send(block.clone());
                tokio::task::yield_now().await;
            }
            Ok((blocks, stop_reason))
        }
    }

    #[tokio::test]
    async fn test_streaming_tool_execution() {
        let dir = std::env::temp_dir().join("loopr-streaming-test");
        let _ = std::fs::create_dir_all(&dir);

        let llm = StreamingMockLlm::new(vec![
            // First: tool use (sent via streaming channel)
            (
                vec![ContentBlock::ToolUse {
                    id: "tu-1".to_string(),
                    name: "shell".to_string(),
                    input: serde_json::json!({"command": "echo streamed"}),
                }],
                Some(crate::tools::types::StopReason::ToolUse),
            ),
            // Second: final text
            (
                vec![ContentBlock::Text {
                    text: "Streaming done.".to_string(),
                }],
                Some(crate::tools::types::StopReason::EndTurn),
            ),
        ]);
        let executor = ToolExecutor::standard(&[]);
        let ctx = ToolContext::new(dir.clone(), "test-stream".into()).with_deny_patterns(vec![]);

        let result = run_tool_loop(&llm, &executor, &ctx, "system", user_message("go"), 10, None, None)
            .await
            .unwrap();

        assert_eq!(result.text, "Streaming done.");
        assert_eq!(result.tool_calls_count, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_complete_streaming_default_impl() {
        // The default impl on MockAgenticLlm should work via fallback
        let llm = MockAgenticLlm::new(vec![(
            vec![
                ContentBlock::Text { text: "hi".to_string() },
                ContentBlock::ToolUse {
                    id: "tu-1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": "test.txt"}),
                },
            ],
            Some(crate::tools::types::StopReason::ToolUse),
        )]);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let (blocks, stop) = llm.complete_streaming("sys", &[], &[], tx).await.unwrap();

        assert_eq!(blocks.len(), 2);
        assert_eq!(stop, Some(crate::tools::types::StopReason::ToolUse));

        // Channel should have received both blocks
        let mut received = Vec::new();
        while let Ok(block) = rx.try_recv() {
            received.push(block);
        }
        assert_eq!(received.len(), 2);
    }
}
