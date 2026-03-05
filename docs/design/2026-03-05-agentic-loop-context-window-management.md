# Design Document: Agentic Loop Context Window Management

**Author:** Scott Idler + Claude
**Date:** 2026-03-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The agentic tool loop (`run_tool_loop`) accumulates all messages — including full file contents from tool results — and sends them verbatim to the Anthropic API on every iteration. With no context window management, the prompt grows unboundedly until it exceeds the API's 200K token input limit, crashing the session. This design adds three capabilities modeled after Claude Code's architecture: (1) a **subagent delegation tool** that runs heavy research in a child loop with its own context window, (2) **LLM-powered auto-compaction** that summarizes old context before discarding it, and (3) **defensive caps** on individual tool outputs. Together these allow Loopr to handle bulk operations like "read every .md file in ~/pd/" the same way Claude Code does — the parent never sees the raw data, only the child's distilled findings.

## Problem Statement

### Background

Loopr's agentic loop (`src/tools/agentic_loop.rs`) drives all LLM-tool interactions: Chat mode, Implementer, Reviewer, Researcher, and Coordinator. Each iteration appends an assistant message (LLM response) and a user message (tool results) to the conversation history. The full history is sent to the API on every call.

Existing infrastructure handles token budgeting for the **initial user message** (via `ContextBuilder` and `TokenBudget`), but there is no management of the **cumulative conversation history** that grows across loop iterations.

### Problem

When an agent (or the user in Chat mode) makes repeated tool calls — especially `read` calls on large files — the conversation history grows without bound. A session that reads 10-15 source files can easily exceed 400K tokens, far above the 200K input limit for `claude-sonnet-4-6` and `claude-opus-4-6`.

**Observed failure:** Chat mode hit 473,866 tokens after ~12 `read` tool calls, producing: `"prompt is too long: 473866 tokens > 200000 maximum"`.

**Root cause analysis — three compounding factors:**
1. **ReadTool has no output cap** — returns entire files regardless of size (no default line limit)
2. **No per-tool-result truncation** — tool results are stored verbatim in messages
3. **No conversation-level compaction** — all messages are sent to the API forever
4. **No subagent delegation** — every tool call runs inline, dumping raw content into the parent's context

The shell tool and grep tool have some limits (16 KB cap, `head -100`), but `read` is the primary offender since agents use it heavily.

**Why simple truncation isn't enough:** Consider "read every .md file in ~/pd/" — that's 50 files, 22K lines, 1.2 MB. Even with a 500-line read cap, the LLM calls `read` 50 times. Dumb compaction (replace old results with "[truncated]") means by file #30, the first 20 files are gone. The LLM can't give a useful answer about all 50 files because it's forgotten most of them. Claude Code solves this with subagents: a child process reads all 50 files in its own context, distills findings, and returns a short summary. The parent's context never sees the raw file contents.

### Goals

- **Primary:** Subagent delegation — heavy tool work runs in a child loop with its own context window, returning distilled results to the parent
- **Primary:** LLM-powered auto-compaction — when context grows, summarize old messages before discarding them (preserve knowledge, free tokens)
- **Secondary:** Defensive caps — prevent individual tool results from being excessively large
- Prevent API "prompt too long" errors under all usage patterns
- Work transparently — the LLM decides when to delegate vs. run inline

### Non-Goals

- Full Researcher-style agent spawning via daemon IPC (that's the heavy path; we want a lightweight in-process child loop)
- Counting exact tokens via tiktoken or the API's token counting endpoint (approximation is sufficient)
- Changing the ContextBuilder or TokenBudget for initial prompt assembly (those work fine)
- Reducing max_iterations for any agent role

## Proposed Solution

### Overview

Four layers, inspired by Claude Code's architecture. The first two are the smart path (prevent the problem); the last two are the safety net (handle what leaks through).

| Layer | Where | What | Claude Code Equivalent |
|-------|-------|------|----------------------|
| **1. Subagent delegation** | New `delegate` tool | Child `run_tool_loop` with own context; returns summary to parent | Agent/Explore tool |
| **2. Auto-compaction** | `agentic_loop.rs` | LLM summarizes old messages before discarding | Auto-compaction |
| **3. Per-result cap** | `agentic_loop.rs` | Cap each tool result at 32 KB | N/A (built-in to tools) |
| **4. ReadTool cap** | `read.rs` | Default max 500 lines per read | N/A (Read tool has built-in limit) |

### Architecture

```
┌─ Parent run_tool_loop ─────────────────────────────────────────────┐
│                                                                     │
│  User: "read every .md file in ~/pd/ and summarize"                │
│                                                                     │
│  LLM decides: this is a bulk operation → calls `delegate` tool     │
│       │                                                             │
│       ▼                                                             │
│  ┌─ delegate tool ──────────────────────────────────────────────┐  │
│  │                                                               │  │
│  │  Spawns child run_tool_loop:                                  │  │
│  │  • Own message history (starts empty)                         │  │
│  │  • Own context window (full 200K budget)                      │  │
│  │  • Same ToolExecutor (read, grep, shell, etc.)                │  │
│  │  • Cheaper/faster model (haiku for research tasks)            │  │
│  │  • System prompt: "Complete this task. Return findings."      │  │
│  │                                                               │  │
│  │  Child reads files, searches, analyzes...                     │  │
│  │  Child hits own context limit → own compaction handles it     │  │
│  │  Child returns: AgenticResult { text: "summary of findings" } │  │
│  │                                                               │  │
│  └───────────────────────────────────────────────────────────────┘  │
│       │                                                             │
│       ▼                                                             │
│  ToolResult { content: "Here's what I found: ..." }  ← SHORT      │
│  (only the summary enters parent's message history)                 │
│                                                                     │
│  If context still grows over time:                                  │
│  auto-compaction summarizes old messages before discarding          │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### How the Layers Interact

```
User: "read every .md in ~/pd/ and summarize"

1. LLM sees `delegate` tool in its tool list
   → Decides this is bulk work → calls delegate(task="read all .md files...")
   → Child loop runs: reads 50 files, hits its own compaction, returns summary
   → Parent sees: ToolResult { content: "Found 50 files. Key themes: ..." }  (small)
   → Parent's context barely grew. Done.

User: "now also check the config files and the README"
   → LLM reads 3-4 files inline (they're small, no delegation needed)
   → Context grows modestly. No compaction triggered. Fine.

User continues chatting for 30 more exchanges with tool calls...
   → Context gradually grows from accumulated tool results
   → At ~150K tokens: auto_compact fires
   → Summarizes old messages (including that delegate result from earlier)
   → Frees ~80K tokens. Conversation continues.

Fallback: LLM doesn't use delegate (reads files inline instead)
   → Layer 3 (per-result cap) prevents any single read from being huge
   → Layer 4 (read cap) limits to 500 lines per file
   → Layer 2 (auto-compaction) fires when total exceeds 150K
   → Summarizes old file reads, preserves key findings
   → Still works, just less efficiently than delegation
```

### Layer 1: Subagent Delegation (`delegate` tool)

**New file:** `src/tools/builtin/delegate.rs`

A new built-in tool that spawns a child `run_tool_loop` with its own context window. The parent LLM calls it when it needs to do bulk operations, deep research, or any task that would consume too much context.

**Tool definition:**
```rust
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
    fn name(&self) -> &str { "delegate" }

    fn description(&self) -> &str {
        "Delegate a task to a subagent with its own context window. \
         Use this for bulk operations (reading many files, searching large codebases) \
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
            None => return ToolResult { content: "missing required parameter: task".into(), is_error: true },
        };
        let max_iter = input.get("max_iterations")
            .and_then(|v| v.as_u64())
            .unwrap_or(20) as u32;

        // Create child context with unique exec_id
        let child_id = format!("{}:sub-{}", ctx.exec_id, uuid_short());
        let child_ctx = ToolContext::new(ctx.working_dir.clone(), child_id)
            .with_sandbox(ctx.sandbox_enabled);

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

        match run_tool_loop(
            self.llm.as_ref(),
            self.executor.as_ref(),
            &child_ctx,
            &system_prompt,
            messages,
            max_iter,
            None, // Child events don't stream to TUI (could be wired up later)
        ).await {
            Ok(result) => ToolResult {
                content: result.text,
                is_error: false,
            },
            Err(e) => ToolResult {
                content: format!("subagent failed: {}", e),
                is_error: true,
            },
        }
    }
}
```

**Key design decisions:**

- **Same `ToolExecutor`** — the child has access to all the same tools (read, grep, shell, etc.). It can even call `delegate` recursively (with depth limits, see Edge Cases).
- **Own message history** — starts fresh with just the task as a user message. The child's 200K context window is entirely its own.
- **Own `ToolContext`** — unique `exec_id` for logging/tracking, same `working_dir` for file access.
- **Returns text only** — the child's `AgenticResult.text` (the LLM's final non-tool response) becomes the tool result. All the intermediate file reads, tool calls, and reasoning stay in the child's context and are discarded.
- **No event streaming** — child tool events don't appear in the TUI. This keeps the parent's Chat view clean. Can be wired up later with a child event channel if needed.

**Integration into ToolExecutor:**

The `delegate` tool needs `Arc<dyn AgenticLlm>` and `Arc<ToolExecutor>`, which the standard `ToolExecutor::standard()` and `ToolExecutor::chat()` constructors don't currently provide. Two options:

**Option A: Constructor injection** (preferred)
```rust
impl ToolExecutor {
    pub fn chat_with_delegation(
        configured: &[ToolEntry],
        llm: Arc<dyn AgenticLlm>,
    ) -> Self {
        let mut exec = Self::chat(configured);
        let delegate = DelegateTool::new(llm, Arc::new(Self::chat(configured)));
        exec.tools.insert("delegate".to_string(), Box::new(delegate));
        exec
    }
}
```

**Option B: Lazy initialization** — register the delegate tool after the executor is created. Less clean but avoids the circular reference (executor contains delegate which contains executor).

Option A is cleaner because the child executor is a separate instance (no circular Arc). The child's executor does NOT include the `delegate` tool by default, preventing unbounded recursion. If recursive delegation is needed, it can be explicitly enabled with a depth counter.

**Where to wire it up — TUI Chat:**

In `src/tui/run.rs`, the chat mode already creates `Arc<AgentLlmClient>` and `Arc<ToolExecutor>`. Change:
```rust
// Before:
let tool_executor = Arc::new(ToolExecutor::chat(&[]));

// After:
let tool_executor = Arc::new(ToolExecutor::chat_with_delegation(&[], Arc::clone(&llm)));
```

**Where to wire it up — Agent loops:**

Agents (Implementer, Researcher, Coordinator) that use `run_tool_loop` can similarly get delegation by injecting the delegate tool into their executors. This is optional — agents already have scoped context via `ContextBuilder`. But the Coordinator (with `max_iterations: u32::MAX`) would benefit most.

### Layer 2: Auto-Compaction with LLM Summarization

**File:** `src/tools/agentic_loop.rs`

When the conversation approaches the context limit, use a fast/cheap LLM call to summarize old messages before discarding them. This preserves knowledge (unlike dumb truncation) while freeing tokens.

**Constants:**
```rust
/// Trigger compaction when estimated input tokens exceed this threshold.
/// Set at 75% of the 200K context window to leave room for output tokens,
/// tool definitions (~2K tokens), and estimation error margin.
const COMPACTION_THRESHOLD: usize = 150_000;

/// Minimum number of messages to keep uncompacted (most recent).
/// These are the "working set" the LLM needs for its current task.
const PROTECTED_TAIL_MESSAGES: usize = 6; // Last 3 pairs (assistant + user)
```

**Algorithm:**

```rust
async fn auto_compact(
    llm: &dyn AgenticLlm,
    system_prompt: &str,
    messages: &mut Vec<Message>,
) {
    let total = estimate_tokens(system_prompt) + estimate_message_tokens(messages);
    if total <= COMPACTION_THRESHOLD {
        return;
    }

    // Split messages into compactable (old) and protected (recent).
    // Adjust split so the protected tail starts with an "assistant" message,
    // maintaining the user/assistant alternation after we prepend the summary (user).
    let mut split = messages.len().saturating_sub(PROTECTED_TAIL_MESSAGES);
    if split == 0 {
        return; // Nothing old enough to compact
    }
    // Walk backward to find an assistant message boundary
    while split > 0 && messages[split].role != "assistant" {
        split -= 1;
    }
    if split == 0 {
        return; // Can't find a safe split point
    }

    let old_messages = &messages[..split];

    // Build a summary of the old conversation
    let summary_prompt = "You are a context compactor. Summarize the following conversation \
        history into a concise recap. Preserve: key findings, file contents that were analyzed, \
        decisions made, and any state the assistant needs to continue its work. \
        Be factual and structured. Use bullet points.";

    let summary_input = format_messages_for_summary(old_messages);

    // Use the same LLM but with a compact system prompt
    let summary_messages = vec![Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: format!("Summarize this conversation history:\n\n{}", summary_input),
        }],
    }];

    let summary_result = llm.complete(summary_prompt, &summary_messages, &[]).await;

    match summary_result {
        Ok((blocks, _)) => {
            let summary_text = extract_text(&blocks);

            // Replace old messages with a single summary message
            let summary_message = Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: format!(
                        "[Context compacted — summary of {} earlier messages]\n\n{}",
                        split, summary_text
                    ),
                }],
            };

            // Rebuild: summary + protected tail
            let protected = messages[split..].to_vec();
            messages.clear();
            messages.push(summary_message);
            messages.extend(protected);

            debug!(
                "auto_compact: compacted {} messages into summary ({} tokens), {} protected",
                split,
                estimate_tokens(&summary_text),
                protected.len()
            );
        }
        Err(e) => {
            // Fallback: dumb truncation if summarization fails
            warn!("auto_compact: summarization failed ({}), falling back to truncation", e);
            fallback_truncate(messages, split);
        }
    }
}

/// Fallback: replace old tool results with placeholders (no LLM needed).
fn fallback_truncate(messages: &mut [Message], compactable_end: usize) {
    for msg in messages[..compactable_end].iter_mut() {
        for block in msg.content.iter_mut() {
            if let ContentBlock::ToolResult { content, .. } = block {
                if estimate_tokens(content) > 50 {
                    *content = "[earlier content truncated — re-read if needed]".to_string();
                }
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
                    // Truncate very large results for the summarizer too
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
```

**Integration into `run_tool_loop`:**

```rust
// At the top of each iteration, before llm.complete():
auto_compact(llm, system_prompt, &mut messages).await;
```

**Key design decisions:**

- **Uses the same LLM instance** — no need for a separate client. The summarization call uses the same model and API key. For cost optimization, a future improvement could use a cheaper model (haiku-class).
- **Fallback to dumb truncation** — if the summarization call itself fails (rate limit, network error), we fall back to placeholder truncation. The loop never crashes from context overflow.
- **Summary replaces old messages structurally** — the summary is a single `user` message with a `Text` block. The split point must be chosen to ensure the protected tail starts with an `assistant` message, preserving the API's alternating user/assistant invariant. The algorithm adjusts the split point to the nearest assistant message boundary.
- **Protected tail of 6 messages (3 pairs)** — more generous than the dumb truncation approach. The LLM keeps full context for its last 3 tool interactions.
- **Summary input is itself truncated** — large tool results in old messages are capped at 4K chars for the summarizer. We don't need the summarizer to read entire files either.

### Layer 3: Per-Tool-Result Cap

**File:** `src/tools/agentic_loop.rs`

After executing each tool call, cap the result content at 32 KB (~8K tokens):

```rust
const MAX_TOOL_RESULT_CHARS: usize = 32_768;

fn cap_tool_result(content: String) -> String {
    if content.len() <= MAX_TOOL_RESULT_CHARS {
        return content;
    }
    let truncated = &content[..MAX_TOOL_RESULT_CHARS];
    if let Some(pos) = truncated.rfind('\n') {
        format!("{}\n... [output truncated at ~{} chars]", &truncated[..pos], MAX_TOOL_RESULT_CHARS)
    } else {
        format!("{}\n... [output truncated]", truncated)
    }
}
```

Applied in `run_tool_loop` after tool execution, before appending to messages.

### Layer 4: ReadTool Output Cap

**File:** `src/tools/builtin/read.rs`

Default max 500 lines when neither `offset` nor `limit` is specified:

```rust
let limit = input.get("limit").and_then(|v| v.as_u64()).map(|l| l as usize);
let effective_limit = limit.unwrap_or(500);

let end = (start + effective_limit).min(lines.len());

// Append truncation note if file was longer
if end < lines.len() && limit.is_none() {
    numbered.push(format!(
        "\n... [{} more lines, use offset/limit to paginate]",
        lines.len() - end
    ));
}
```

Update tool description to document the default limit.

### Token Estimation Helpers

```rust
use crate::agents::context::estimate_tokens;

fn estimate_message_tokens(messages: &[Message]) -> usize {
    messages.iter().map(|m| {
        m.content.iter().map(|block| match block {
            ContentBlock::Text { text } => estimate_tokens(text),
            ContentBlock::ToolUse { name, input, .. } => {
                estimate_tokens(name) + estimate_tokens(&input.to_string())
            }
            ContentBlock::ToolResult { content, .. } => estimate_tokens(content),
        }).sum::<usize>()
        + 10 // overhead per message (role, structural JSON)
    }).sum()
}
```

### Data Model

**New types:**
- `DelegateTool` struct in `src/tools/builtin/delegate.rs`

**Modified types:**
- `ToolExecutor` — new constructor `chat_with_delegation()` that includes the delegate tool
- `ReadTool::execute()` — default 500-line cap
- `run_tool_loop()` — add `auto_compact()` call before `llm.complete()`, add `cap_tool_result()` on results

### Implementation Plan

**Phase 1: Defensive caps (immediate crash prevention)**
Files: `src/tools/builtin/read.rs`, `src/tools/agentic_loop.rs`
- Add 500-line default to ReadTool
- Add `cap_tool_result()` (32 KB cap)
- Add `estimate_message_tokens()` helper
- Tests for all three

**Phase 2: Subagent delegation**
Files: `src/tools/builtin/delegate.rs` (new), `src/tools/builtin/mod.rs`, `src/tools/executor.rs`, `src/tui/run.rs`
- Implement `DelegateTool`
- Add `chat_with_delegation()` constructor to `ToolExecutor`
- Wire into TUI Chat mode
- Tests: mock LLM, verify child loop runs independently, verify only summary enters parent messages

**Phase 3: Auto-compaction with summarization**
Files: `src/tools/agentic_loop.rs`
- Implement `auto_compact()` with LLM summarization
- Implement `fallback_truncate()` for error case
- Wire into `run_tool_loop()` before each `llm.complete()` call
- Tests: mock conversations over budget, verify summarization is called, verify fallback works

**Phase 4: Observability**
Files: `src/tools/agentic_loop.rs`, `src/ipc/protocol.rs`
- Log token estimates at each iteration
- Emit `DaemonEvent` when compaction occurs
- Emit `DaemonEvent` when delegate tool spawns/completes child
- TUI shows compaction/delegation events in Chat

**Phase 5: Agent integration (optional)**
Files: `src/agents/executor.rs`, agent-specific files
- Wire `delegate` tool into Implementer and Coordinator agent executors
- Coordinator can delegate research tasks to child loops instead of spawning full Researcher agents

## Alternatives Considered

### Alternative 1: Dumb Truncation Only (No Summarization, No Subagents)

- **Description:** When over budget, replace old tool results with "[truncated]" placeholder.
- **Pros:** Simple. No extra LLM calls.
- **Cons:** Data loss. The LLM loses knowledge of what it read earlier. For "read all files" use cases, the LLM forgets most of what it read and can't give a useful answer.
- **Why not chosen:** This was the original v1 of this design. It prevents crashes but doesn't match Claude Code's capability. The user correctly identified that Loopr should handle bulk operations the way Claude Code does — with intelligence, not data loss.

### Alternative 2: Full Researcher Agent Spawning via Daemon IPC

- **Description:** Use the existing `SpawnResearcher` action and daemon IPC to spawn a full Researcher agent for delegation.
- **Pros:** Already implemented. Battle-tested IPC path.
- **Cons:** Heavy — requires daemon message routing, AgentIpcBridge, session tracking, TaskStore polling. Overkill for "read these files and tell me what you found." The Researcher agent has its own action parsing, iteration loop, and error handling that's more complex than needed.
- **Why not chosen:** A lightweight in-process child `run_tool_loop` is simpler, faster, and sufficient. The existing Researcher infrastructure is designed for autonomous research tasks with inter-agent communication, not for tool delegation.

### Alternative 3: Sliding Window (Drop Old Messages Entirely)

- **Description:** Keep only the last N messages, dropping older ones entirely.
- **Pros:** Simple. Guaranteed to stay within budget.
- **Cons:** Loses conversation coherence. ToolUse blocks without matching ToolResult blocks cause API errors.
- **Why not chosen:** Message pairs must stay paired for the API. Dropping messages breaks this invariant.

### Alternative 4: Exact Token Counting via API

- **Description:** Use Anthropic's token counting API to get exact counts before sending.
- **Pros:** Exact, no approximation errors.
- **Cons:** Extra API call per iteration. Adds latency.
- **Why not chosen:** The ~4 chars/token approximation is sufficient. We target 150K vs 200K limit — the 25% margin absorbs estimation errors.

## Technical Considerations

### Dependencies

- `estimate_tokens()` from `src/agents/context.rs` — already `pub`
- `run_tool_loop()` already exists and is reusable for child loops
- `Arc<dyn AgenticLlm>` — `AgenticLlm` is already `Send + Sync`
- No new external crate dependencies

### Performance

- **Subagent delegation:** Adds latency for spawning a child loop, but the child runs concurrently and the parent awaits. For bulk operations, this is much faster than the parent reading everything sequentially (the parent would hit context limits and crash anyway).
- **Auto-compaction:** One extra LLM call when triggered. This is the cost of preserving knowledge. The call uses the same model but with a small prompt (summary of old messages, not the full context). Typically completes in 2-5 seconds.
- **Token estimation:** O(n) in total content, called once per iteration. Negligible vs HTTP round-trip.

### Security

- **Delegate tool:** Child loop runs with the same sandbox settings as parent. Same deny patterns, same working directory. No privilege escalation.
- **Auto-compaction:** The summarization prompt is hardcoded, not user-controlled. No prompt injection risk from tool results because the summarizer treats them as data to summarize, not instructions to follow.

### Testing Strategy

- **DelegateTool unit tests:** Mock `AgenticLlm`, verify child loop runs, verify result is returned as ToolResult, verify child context is isolated
- **DelegateTool integration test:** Mock LLM that reads files in child, verify parent only sees summary text
- **Auto-compaction unit tests:** Under budget (no-op), over budget (summarization called), fallback on summarization failure
- **Cap tests:** `cap_tool_result()` under/over limit, ReadTool 500-line default
- **End-to-end test:** Mock LLM with 50 file reads, verify no "prompt too long" error, verify final answer includes knowledge from early files

### Rollout Plan

Phases 1-3 are internal to the agentic loop and tool system. No IPC, daemon, or config changes needed (Phase 1 and 3). Phase 2 adds a new tool and modifies executor construction in TUI — minor wiring.

Phase 4 adds events. Phase 5 is optional agent integration.

All existing tests pass unchanged — new functionality is additive.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Summarization LLM call fails (rate limit, network) | Medium | Low | Fallback to dumb truncation. Loop never crashes. |
| Summarization loses critical detail | Low | Medium | Protected tail (last 3 pairs) keeps recent context. LLM can re-read files. Summary prompt emphasizes key findings. |
| Delegate tool called recursively (child spawns child) | Low | Medium | Child executor doesn't include `delegate` tool by default. Explicit opt-in for recursion with depth limit. |
| Delegate tool cost (extra LLM calls for child) | Medium | Low | Child uses same model. For cost optimization, future work can use haiku-class for child. User sees tool call in Chat, can cancel. |
| Token estimate inaccuracy causes overshoot | Medium | Low | 25% margin (150K budget vs 200K limit). |
| ReadTool 500-line cap too restrictive | Low | Low | LLM can use offset/limit. Delegate tool bypasses this by having its own context. |
| Summarization call itself exceeds context limit | Low | Medium | `format_messages_for_summary()` truncates large tool results to 4K chars. Summary input is bounded. |

## Edge Cases

### Recursive delegation
Child executor does not include the `delegate` tool, preventing unbounded recursion. If future use cases require nested delegation, add an explicit depth counter passed through `ToolContext`.

### Child loop hits its own context limit
The child runs the same `run_tool_loop`, which calls `auto_compact`. So the child manages its own context independently. If the child's summarization also fails, the fallback truncation ensures the child never crashes — it may lose context but will return *something*.

### Concurrent delegate calls in one iteration
If the LLM emits two `delegate` tool calls in a single response, both children run sequentially (the current tool execution loop processes calls serially). This avoids concurrent LLM rate limit pressure. If we parallelize tool execution in the future, delegate calls should be serialized or rate-limited.

### Delegate tool and file writes
The child has write access to the same `working_dir` as the parent. For safety, the delegate tool's system prompt emphasizes read-only research. A future hardening could give the child a read-only `ToolExecutor` that excludes `write`, `edit`, and `shell` tools. For now, this is a prompt-level constraint.

### Auto-compaction on first message
If the user's very first message contains a massive prompt (e.g., pasted file contents), `auto_compact` could fire before any tool calls. The split-point algorithm handles this: if there are no assistant messages to split on, compaction is skipped and the per-result cap (Layer 3) is the only defense.

### Chat mode: compaction across submissions
`canonical_messages` carries full history across user submissions. Each submission's `run_tool_loop` call gets these messages as input. `auto_compact` fires at the top of the loop, compacting old messages from *prior submissions*. This means the first LLM call of a new submission may trigger compaction of the previous submission's tool results — which is the correct behavior.

## Open Questions

- [ ] Should the delegate tool use a cheaper model (haiku-class) for the child loop? Saves cost but reduces capability.
- [ ] Should auto-compaction use a cheaper model than the parent? (e.g., parent is opus, compactor is haiku)
- [ ] Should the delegate tool support streaming child events to the TUI? (Would show "subagent: reading file X" in Chat)
- [ ] Should `MAX_TOOL_RESULT_CHARS` and `COMPACTION_THRESHOLD` be configurable in `loopr.toml`?
- [ ] Should the delegate tool be available to all agents (Implementer, Coordinator) or only Chat mode?
- [ ] Should compaction history be persisted for diagnostics? (e.g., "this session compacted 3 times, lost N tokens of context")

## References

- Observed failure: Chat mode, 473K tokens > 200K limit, ~12 `read` tool calls
- Claude Code architecture: subagents (Agent/Explore tool), auto-compaction
- Existing token budgeting: `src/agents/context.rs` — `estimate_tokens()`, `truncate_prose()`, `TokenBudget`
- Existing shell output cap: `src/tools/shell.rs` — `MAX_OUTPUT = 16 * 1024`
- Existing Researcher agent: `src/agents/researcher.rs` — closest existing subagent pattern
- Anthropic API limits: `claude-sonnet-4-6` and `claude-opus-4-6` — 200K input token limit
- MVP4 design doc: `docs/design/2026-02-26-loopr-v3-mvp4.md` — context builder and token budgeting
- `~/pd/` test case: 50 .md files, 22K lines, 1.2 MB — representative bulk read scenario
