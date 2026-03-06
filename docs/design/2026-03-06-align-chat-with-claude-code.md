# Design Document: Align Loopr Chat with Claude Code's Agentic Loop

**Author:** Claude (Opus 4.6), with strategic input from Gemini
**Date:** 2026-03-06
**Status:** Implemented
**Review Passes Completed:** 4/5

## Summary

Reverse-engineering Claude Code v2.1.70 revealed specific gaps between loopr's `run_tool_loop` and Claude Code's proven agentic loop (`ry` → `ea1`). Two independent analyses (Claude and Gemini) converged on the same diagnosis: the loop architecture is correct, but tuning and missing features cause poor chat UX. This document proposes targeted changes across two phases — immediate wins in the core loop, and a future multi-agent IPC upgrade — to close those gaps.

## Problem Statement

### Background

Two independent analyses (by Claude and Gemini) confirmed that loopr's `run_tool_loop` architecture is fundamentally correct — it matches Claude Code's core pattern: call model → execute tools → feed results back → repeat. The floundering in recent chat sessions was not an architectural failure but a **tuning and missing-feature problem**.

Research documents:
- `docs/research/2026-03-05-claude-code-agentic-loop-by-claude.md`
- `docs/2026-03-06-research-claude-code-chat-loop-by-gemini.md`
- `docs/research/2026-03-05-claude-vs-gemini-comparison-by-claude.md`

### Problem

Six specific gaps cause loopr's chat to feel slow and wasteful compared to Claude Code:

1. **Chat iteration cap too high** — `max_iterations=10` gives the model permission to loop endlessly through sequential tool calls. Claude Code constrains interactive chat more tightly, forcing the model to batch or delegate.

2. **System prompt not aggressive enough about parallel tool calls** — Claude Code's prompts heavily reinforce batching multiple tool calls in a single turn. Loopr's prompt mentions `delegate` but doesn't enforce single-turn batching for the tools the model calls directly.

3. **No max_tokens recovery** — if the model hits the output token limit mid-response, loopr treats it as a final response. Claude Code injects a "resume mid-thought" message and continues the loop.

4. **No microcompact pass** — Claude Code runs a lightweight per-turn pruning step (microcompact) before the heavier autocompact. Loopr only has the heavy path, which means oversized tool results persist until the 150K threshold triggers full compaction.

5. **No streaming tool execution** — loopr waits for the model to finish its entire response before executing any tools. Claude Code starts executing tools as soon as each `tool_use` block completes in the SSE stream, overlapping tool execution with model generation.

6. **No multi-agent mailbox pattern** — Claude Code uses a structured mailbox system for inter-agent messaging (`teammate_mailbox`) with priority queues for shutdown and idle notifications. Loopr's broadcast channel is fire-and-forget — agents can't send structured messages to each other.

### Goals

- Close gaps 1-4 immediately with targeted changes to the core loop
- Lay groundwork for gaps 5-6 as a follow-on phase
- Improve perceived chat responsiveness without architectural upheaval
- Maintain backward compatibility with agent/delegate loops

### Non-Goals

- Daemon process architecture changes
- TUI rendering changes (streaming tool execution will use existing event infrastructure)
- Cowork/multi-pane orchestration (Claude Code's conductor pattern)
- New tools or tool API changes

## Proposed Solution

### Overview

Six changes across two phases, ordered by priority:

| # | Phase | Change | Files | Complexity |
|---|-------|--------|-------|------------|
| 1 | 1 | Lower chat iteration cap | `config.rs` | Trivial |
| 2 | 1 | Stronger parallel tool prompt | `chat.rs` | Trivial |
| 3 | 1 | Max tokens recovery | `agentic_loop.rs` | Low |
| 4 | 1 | Microcompact pass | `agentic_loop.rs` | Medium |
| 5 | 2 | Streaming tool execution | `agentic_loop.rs`, `llm.rs` | High |
| 6 | 2 | Agent mailbox IPC | `ipc/protocol.rs`, `daemon/handlers.rs` | High |

**Phase 1** (Changes 1-4) ships first — immediate UX improvements, no interface changes.
**Phase 2** (Changes 5-6) ships after — requires new traits and IPC protocol additions.

---

### Change 1: Lower Chat Iteration Cap

**What Claude Code does:** Constrains interactive chat iterations tightly. The model either gathers context efficiently in 1-2 turns or fails fast.

**What loopr does today:** `ChatConfig::default()` sets `max_iterations = 10`.

**Proposed change:** Lower to `max_iterations = 3`.

Gemini recommended 3. My initial draft proposed 5 as a compromise. After reviewing the actual flow with parallel tool execution and delegate, 3 is correct:

- Turn 1: model emits parallel tool calls (read 5 files, grep 3 patterns — all at once)
- Turn 2: model reads results, maybe one follow-up tool call
- Turn 3: model responds to the user

If the model can't answer in 3 turns, it should be using `delegate` for the heavy lifting. The tight cap is the **forcing function** that drives better tool strategy. With `max_iterations=10`, the model has no pressure to batch.

```rust
// In ChatConfig::default():
max_iterations: 3,
```

Keep delegate at 20. Keep agent roles at their existing values.

**Test updates:** Update `test_chat_config_defaults` and `test_chat_config_to_role_config` to assert `max_iterations == 3`.

---

### Change 2: Stronger Parallel Tool Prompt

**What Claude Code does:** The system prompt aggressively commands parallel tool execution and tells the model about its constraints.

**What loopr does today:** Mentions `delegate` for bulk operations but doesn't tell the model it CAN emit multiple `tool_use` blocks in a single response, and doesn't mention the iteration cap.

**Proposed change:**

```rust
pub const CHAT_SYSTEM_PROMPT: &str = "\
You are an AI assistant embedded in the Loopr development orchestrator. \
You help the user explore ideas, discuss architecture, and plan changes to their codebase. \
When the user is ready to formalize a plan, they will type /plan.\n\n\
You have tools available: read, write, edit, grep, glob, find, list, tree, shell, search, fetch, and delegate.\n\n\
TOOL STRATEGY — READ CAREFULLY:\n\
- You can call MULTIPLE tools in a SINGLE response. If you need to read 5 files, \
  emit 5 read tool_use blocks in ONE response. They execute in parallel.\n\
- You have a MAXIMUM of 3 tool iterations. Each time you call tools counts as one. \
  Maximize every turn — batch ALL independent tool calls together.\n\
- Use `delegate` for tasks requiring more than 5 tool calls or deep multi-step research. \
  Delegate spawns a subagent with its own context window and 20 iterations.\n\
- Do NOT step through files one at a time. Do NOT retry failed searches sequentially.\n\
- Use `shell` for system commands when no built-in tool fits.\n\n\
Be concise and direct. Act on user requests immediately using tools — don't ask for permission.";
```

Key changes from current prompt:
- Explicit "MULTIPLE tools in a SINGLE response" with parallel execution noted
- Iteration cap stated directly so the model knows it's constrained
- Structured "TOOL STRATEGY" section instead of buried paragraphs
- Removed redundant guidance (the old prompt said the same thing about delegate three times)

**Test updates:** Update `test_system_prompt_chat` assertions.

---

### Change 3: Max Tokens Recovery

**What Claude Code does:** When `stop_reason == "max_tokens"`, injects a synthetic user message — *"Output token limit hit. Resume directly — no apology, no recap. Pick up mid-thought. Break remaining work into smaller pieces."* — and continues the loop. Up to N recovery attempts (observed constant `sa1` in the binary).

**What loopr does today:** `StopReason::MaxTokens` already exists in the enum (`src/tools/types.rs:46`) but `run_tool_loop` treats it the same as `end_turn` — the `if tool_uses.is_empty()` check at line 279 catches it and returns.

**Proposed change:** Add recovery logic after the tool_uses check:

```rust
const MAX_OUTPUT_RECOVERY_ATTEMPTS: u32 = 3;
let mut output_recovery_count: u32 = 0;

// ... inside the for loop, after the tool_uses.is_empty() early-return block:

// Max tokens recovery: if the model hit the output limit, inject a
// "resume" message and continue. Consumes an iteration from the budget.
if stop_reason == Some(StopReason::MaxTokens)
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
                   cut happened. Break remaining work into smaller pieces.".to_string(),
        }],
    });
    if let Some(cb) = on_checkpoint {
        cb(&messages);
    }
    continue;
}
```

**Important detail:** The `continue` skips the tool execution path and goes back to the top of the `for` loop. This means recovery DOES consume an iteration from `max_iterations`. This is intentional — we don't want unbounded recovery. With `max_iterations=3` for chat and a separate `MAX_OUTPUT_RECOVERY_ATTEMPTS=3` cap, the model gets at most 3 recovery attempts within its iteration budget.

**Where in the loop:** This check must go AFTER the `tool_uses.is_empty()` return (line 279) but BEFORE the tool execution block (line 289). Specifically, modify the early return to only trigger on `EndTurn` or `StopSequence`:

```rust
if tool_uses.is_empty() || stop_reason == Some(StopReason::EndTurn) {
    let text = extract_text(&content_blocks);
    return Ok(AgenticResult { text, messages, tool_calls_count: total_tool_calls });
}

// Max tokens recovery (before tool execution)
if stop_reason == Some(StopReason::MaxTokens)
    && output_recovery_count < MAX_OUTPUT_RECOVERY_ATTEMPTS
{
    // ... recovery logic above
}

// If we get here with no tool_uses and no recovery, also return
if tool_uses.is_empty() {
    let text = extract_text(&content_blocks);
    return Ok(AgenticResult { text, messages, tool_calls_count: total_tool_calls });
}
```

**Tests:** Add `test_run_tool_loop_max_tokens_recovery` — mock LLM returns `StopReason::MaxTokens` on first call, `StopReason::EndTurn` on second. Verify the recovery message appears in messages and the loop continues.

---

### Change 4: Microcompact Pass

**What Claude Code does:** Runs a lightweight `microcompact()` at the top of every turn, before the heavier `autocompact()`. Prunes oversized tool results without needing an LLM call.

**What loopr does today:** Only has `auto_compact()` which triggers at 150K tokens and requires an LLM summarization call. The existing `fallback_truncate()` only runs when summarization fails.

**Proposed change:** Extract the truncation logic into a standalone `microcompact()` that runs unconditionally every iteration:

```rust
/// Lightweight per-turn pruning. No LLM call needed.
/// Truncates oversized tool results outside the protected tail.
const MICROCOMPACT_THRESHOLD: usize = 4096;
const MICROCOMPACT_PREVIEW: usize = 2048;

fn microcompact(messages: &mut [Message], protected_tail: usize) {
    let compactable_end = messages.len().saturating_sub(protected_tail);
    if compactable_end == 0 {
        return;
    }

    for msg in messages[..compactable_end].iter_mut() {
        for block in msg.content.iter_mut() {
            if let ContentBlock::ToolResult { content, .. } = block {
                if content.len() > MICROCOMPACT_THRESHOLD {
                    let preview = &content[..MICROCOMPACT_PREVIEW.min(content.len())];
                    let cut = preview.rfind('\n').unwrap_or(preview.len());
                    let original_len = content.len();
                    *content = format!(
                        "{}\n... [truncated from {} chars — re-read if needed]",
                        &preview[..cut],
                        original_len
                    );
                }
            }
        }
    }
}
```

Call order in `run_tool_loop`:

```rust
for iteration in 0..max_iterations {
    microcompact(&mut messages, PROTECTED_TAIL_MESSAGES);  // cheap, every turn
    auto_compact(llm, system_prompt, &mut messages).await;  // expensive, threshold-gated
    // ... rest of loop
}
```

This is sub-millisecond (string truncation only) and prevents the gradual context bloat that eventually triggers the expensive LLM-based compaction.

**Tests:** Add `test_microcompact_truncates_old_results` and `test_microcompact_preserves_protected_tail`.

---

### Change 5: Streaming Tool Execution (Phase 2)

**What Claude Code does:** The `qoH` streaming tool executor receives `tool_use` blocks as they complete during SSE streaming. Tools start executing immediately, overlapping with model generation.

**What loopr does today:** `AgenticLlm::complete()` returns a fully materialized `(Vec<ContentBlock>, Option<StopReason>)`. Tools execute only after the entire response is collected.

**Proposed approach:** Channel-based — more idiomatic in Rust than the closure approach.

1. Add a `complete_streaming` method to `AgenticLlm` that accepts a `tokio::sync::mpsc::UnboundedSender<ContentBlock>` and sends completed blocks as they arrive from SSE.

2. In `run_tool_loop`, spawn a background task that receives blocks from the channel and starts tool execution immediately for `ToolUse` blocks.

3. After `complete_streaming` returns, collect results from already-running tool futures.

```rust
// Sketch — actual implementation needs lifetime management

let (block_tx, mut block_rx) = tokio::sync::mpsc::unbounded_channel();

// Background: start tools as blocks arrive
let executor = Arc::clone(&executor);
let ctx = ctx.clone();
let tool_runner = tokio::spawn(async move {
    let mut handles = Vec::new();
    while let Some(block) = block_rx.recv().await {
        if let ContentBlock::ToolUse { id, name, input } = block {
            let call = ToolCall { id, name, input };
            let exec = Arc::clone(&executor);
            let c = ctx.clone();
            handles.push(tokio::spawn(async move {
                let start = Instant::now();
                let result = exec.execute(&call, &c).await;
                (call, result, start.elapsed().as_millis() as u64)
            }));
        }
    }
    handles
});

// Foreground: stream LLM response, sending blocks to channel
let (content_blocks, stop_reason) = llm
    .complete_streaming(system_prompt, &messages, &tool_defs, block_tx)
    .await?;

// Collect tool results
let handles = tool_runner.await?;
let results = futures::future::join_all(handles).await;
```

**Prerequisites:** `ToolExecutor` must be `Arc`-wrapped or `Clone`. `ToolContext` must be `Clone`. These may require minor refactors.

**Default implementation:** `complete_streaming` gets a default impl that falls back to `complete()` + sending all blocks after the fact, so existing `AgenticLlm` impls don't break.

**Deferred until Phase 2** because Changes 1-4 deliver most of the perceived improvement, and this change touches the LLM interface boundary.

---

### Change 6: Agent Mailbox IPC (Phase 2)

**What Claude Code does:** Uses a `teammate_mailbox` attachment type for structured inter-agent messaging. Messages have priority (shutdown > idle > regular). Agents read from their mailbox with priority ordering.

**What loopr does today:** The `broadcast::Sender<DaemonEvent>` is fire-and-forget. Agents can emit events to the TUI but cannot send structured messages to each other. The Coordinator, Implementer, and Reviewer agents have no direct communication channel.

**Proposed approach:** Introduce a `TaskMailbox` backed by the existing TaskStore:

```rust
/// A structured message between agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: String,
    pub from: String,          // agent ID
    pub to: String,            // agent ID or "broadcast"
    pub priority: MessagePriority,
    pub payload: MessagePayload,
    pub timestamp: i64,
    pub read: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessagePriority {
    Shutdown = 0,  // highest
    Idle = 1,
    Normal = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessagePayload {
    TaskCompleted { task_id: String, status: String },
    ReviewRequested { task_id: String, files: Vec<String> },
    ShutdownRequest { reason: String },
    IdleNotification { summary: String },
    Custom { data: serde_json::Value },
}
```

This formalizes the multi-agent coordination pattern from Claude Code without relying on Anthropic's cloud WebSockets. Messages are stored in TaskStore (JSONL-backed), so they survive daemon restarts.

**Deferred until Phase 2** because it's needed for MVP4's Coordinator/Implementer/Reviewer flow, not for the immediate chat UX fix.

---

## Alternatives Considered

### Alternative 1: Rip out the loop entirely
- **Description:** Abandon the local tool loop; rely on server-side tool execution.
- **Why not chosen:** Physically impossible. The Anthropic API has no access to the user's filesystem. Both research docs confirm CLI agents must loop locally.

### Alternative 2: Replace run_tool_loop with a state machine
- **Description:** Model the loop as explicit states with typed transitions.
- **Why not chosen:** Over-engineering. Claude Code itself uses `while(true)`, not a state machine. The `for` loop is readable and correct.

### Alternative 3: max_iterations = 5 (Claude's initial proposal)
- **Description:** Compromise between current 10 and Gemini's recommended 3.
- **Why not chosen:** 5 is too lenient. It still allows the sequential-tool-call pattern. 3 is the forcing function — it makes parallel batching and delegate mandatory, not optional.

### Alternative 4: WebSocket-based IPC for multi-agent (like Claude Code)
- **Description:** Use WebSockets between agents, mirroring Claude Code's `SessionsWebSocket`.
- **Why not chosen:** Claude Code uses WebSockets because its agents can run on remote servers. Loopr's agents are all local. A TaskStore-backed mailbox is simpler and sufficient.

## Technical Considerations

### Dependencies

No new external dependencies. All changes use existing crates (`tokio`, `futures`, `serde_json`). Phase 2's `Arc<ToolExecutor>` may require adding a `Clone` derive.

### Performance

- **Changes 1-2:** Zero cost. Config + prompt text.
- **Change 3:** Negligible. One string comparison + optional message push per iteration.
- **Change 4 (microcompact):** Sub-millisecond. String truncation, no allocation beyond replacement.
- **Change 5 (streaming tools):** Net positive. Tool latency overlaps with model generation time.
- **Change 6 (mailbox):** Depends on TaskStore write latency (~1ms for JSONL append).

### Testing Strategy

| Change | Test Type | What to Assert |
|--------|-----------|----------------|
| 1 | Unit | `ChatConfig::default().max_iterations == 3` |
| 2 | Unit | `CHAT_SYSTEM_PROMPT` contains "MULTIPLE tools", "3 tool iterations" |
| 3 | Unit | Mock LLM returns `MaxTokens` then `EndTurn` → recovery message in messages, loop continues |
| 4 | Unit | Oversized tool result in old message → truncated after `microcompact()`. Protected tail untouched. |
| 5 | Unit | Mock streaming LLM yields blocks with delays → tools start before `message_stop` |
| 6 | Unit | Agent sends message → recipient reads with priority ordering |

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `max_iterations=3` too tight for some queries | Medium | Medium | Delegate absorbs complex work (20 iterations). Monitor and adjust if needed. |
| Max tokens recovery creates unnecessary iterations | Low | Low | Separate 3-attempt cap. Recovery consumes iteration budget. |
| Microcompact truncates content the model needs | Low | Low | Only truncates outside `PROTECTED_TAIL_MESSAGES`. Adds "[re-read if needed]" hint. |
| Streaming tool execution lifetime issues | High | Low | Deferred to Phase 2. Channel-based approach avoids closure lifetime problems. |
| Stronger prompt causes model to over-batch | Low | Low | Tool result cap (32K chars) and delegate guidance bound the damage. |

## Open Questions

- [ ] Should `max_iterations` be configurable per-session (e.g., user types `/config max_iterations 8`)?
- [ ] Should microcompact thresholds (4096/2048) be constants or configurable?
- [ ] For streaming tool execution: does `ToolExecutor` need `Arc` wrapping or can we use scoped tasks?
- [ ] Should the mailbox be a new TaskStore collection or a separate JSONL file?

## Implementation Phases

### Phase 1: Immediate UX Wins (Changes 1-4)

Ship as a single commit. All changes are in the core loop + config, no interface changes.

1. Lower `ChatConfig::default().max_iterations` to 3
2. Rewrite `CHAT_SYSTEM_PROMPT` with parallel batching guidance and iteration cap
3. Add `StopReason::MaxTokens` recovery logic to `run_tool_loop`
4. Add `microcompact()` and call it per-iteration before `auto_compact`
5. Update all affected tests

### Phase 2: Engine Upgrades (Changes 5-6)

Ship separately. Requires new traits and IPC protocol additions.

1. Add `complete_streaming` to `AgenticLlm` trait with default fallback
2. Implement SSE → content block streaming in the real LLM client
3. Channel-based tool starter in `run_tool_loop`
4. `AgentMessage` / `MessagePriority` / `MessagePayload` types
5. `TaskMailbox` backed by TaskStore
6. Wire mailbox into Coordinator/Implementer/Reviewer agent flow

## References

- `docs/research/2026-03-05-claude-code-agentic-loop-by-claude.md` — Claude's reverse-engineering analysis
- `docs/2026-03-06-research-claude-code-chat-loop-by-gemini.md` — Gemini's analysis and recommendations
- `docs/research/2026-03-05-claude-vs-gemini-comparison-by-claude.md` — Comparison of both analyses
- `src/tools/agentic_loop.rs` — Current loop implementation (lines 212-387)
- `src/domain/chat.rs` — Current chat system prompts
- `src/config.rs` — ChatConfig with `max_iterations` (line 428)
- `src/tools/types.rs` — `StopReason` enum (line 43, `MaxTokens` already exists)
