# Design Document: Chat Agentic Tool Loop

**Author:** Scott Idler
**Date:** 2026-03-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Wire the TUI Chat tab into the unified tool system so the LLM can read files, search code, run commands, and edit files — just like Claude Code, Codex, and Gemini do. Today the Chat tab makes plain text LLM calls with no tools, so the model responds with "I can't access your filesystem." The agentic tool loop (`run_tool_loop`) and `ToolExecutor` already exist but are only used in tests.

## Problem Statement

### Background

MVP3 added the unified tool system: 14 built-in tools, `ToolExecutor`, `ToolContext`, `run_tool_loop`, and the `AgenticLlm` trait. An `AgenticLlm` implementation was added to `AgentLlmClient`. The `ToolExecutor::chat()` factory exists specifically for the TUI chat use case. All the plumbing is in place — but the Chat tab still calls `AgentLlmClient::call_with_history()`, a plain text-only API call that never sends tool definitions.

### Problem

The TUI Chat is the primary user interaction surface in Loopr. When a user asks "read all the .md files here" the LLM responds with "I can't access your filesystem" because it has no tools. This makes the Chat useless for any task that requires codebase interaction — which is most tasks.

### Goals

- Chat LLM calls include tool definitions so the model can invoke tools
- Tool calls are executed locally and results fed back to the LLM (agentic loop)
- Tool invocations are visible in the chat UI (using existing `ChatRole::ToolInvocation`)
- Multi-turn conversation preserves full context including tool call/result history
- Streaming text display continues to work during tool loop iterations

### Non-Goals

- Streaming SSE for tool-use responses (the `AgenticLlm::complete()` uses non-streaming; streaming can be added later)
- Tool approval / confirmation UI (all chat tools execute automatically for now)
- Modifying the agent-side tool system (Implementer, Researcher, etc. are unchanged)
- Adding new tools
- Token budget management for conversation history

## Proposed Solution

### Overview

Replace the Chat tab's plain `call_with_history` with `run_tool_loop`. Maintain a persistent `Vec<Message>` (Anthropic API format with `ContentBlock`s) as the canonical conversation state. The TUI `Vec<ChatMessage>` becomes a display-only view derived from this canonical state.

This is the same architecture used by Claude Code, Codex, and Gemini: the full message history — including `tool_use` and `tool_result` blocks — carries forward across turns, giving the LLM context about prior tool interactions.

### Architecture

**Two parallel data structures for chat:**

| Structure | Purpose | Location |
|-----------|---------|----------|
| `canonical_messages: Vec<Message>` | Anthropic API format. Full history including `ToolUse`/`ToolResult` blocks. Sent to LLM. | `App` field |
| `chat_history: Vec<ChatMessage>` | Display-only. Text summaries for TUI rendering. | `App` field (existing) |

Both are updated together: `canonical_messages` is the source of truth; `chat_history` is derived for display.

**Flow per user message:**

```
1. User types message, presses Enter
   ↓
2. Append Message { role: "user", content: [Text { text }] } to canonical_messages
   Push ChatMessage::user(text) to chat_history
   ↓
3. Clone canonical_messages → owned Vec<Message> for the spawned task
   Set chat_streaming = true
   ↓
4. tokio::spawn:
   │  run_tool_loop(llm, executor, ctx, system_prompt, messages, max_iters, event_tx)
   │    ↓
   │  LLM.complete(system, messages, tools)
   │    ↓
   │  ┌─ If tool_use: execute tools, emit ToolStarted/ToolCompleted events,
   │  │   append results, loop back to LLM.complete()
   │  └─ If end_turn: return AgenticResult
   │    ↓
   │  Send AgenticResult back via oneshot channel
   ↓
5. Event loop receives:
   - LLM text chunks (broadcast) → append to chat_response_buffer (streaming display)
   - ToolStarted/ToolCompleted events (broadcast) → push ChatMessage::tool_invocation()
   - Oneshot result → replace canonical_messages, push ChatMessage::assistant(text),
     emit is_final=true, set chat_streaming = false
```

**Return channel:** The spawned task returns `AgenticResult` via a `tokio::sync::oneshot` channel. The event loop polls this alongside keyboard events and broadcast chunks using `tokio::select!`.

### Data Model

**New field on `App`:**
```rust
/// Canonical conversation in Anthropic API format.
/// Persists across turns, includes tool_use and tool_result blocks.
pub canonical_messages: Vec<crate::tools::types::Message>,
```

**Extended `run_tool_loop` signature:**
```rust
pub async fn run_tool_loop(
    llm: &dyn AgenticLlm,
    executor: &ToolExecutor,
    ctx: &ToolContext,
    system_prompt: &str,
    messages: Vec<Message>,                                    // CHANGED: full prior conversation
    max_iterations: u32,
    event_tx: Option<&broadcast::Sender<DaemonEvent>>,         // NEW: optional tool event channel
) -> eyre::Result<AgenticResult>
```

**Reuse existing event types:**
`AgentEvent::ToolStarted` and `AgentEvent::ToolCompleted` already exist. No new variants needed — just emit these from the agentic loop when an `event_tx` is provided.

**Sharing across spawn boundary:**
`ToolExecutor` and `ToolContext` are `Send + Sync` but not `Clone`. Wrap in `Arc`:
```rust
let tool_executor: Arc<ToolExecutor> = Arc::new(ToolExecutor::chat(&[]));
let tool_ctx: Arc<ToolContext> = Arc::new(ToolContext::new(cwd, "tui-chat".into()));
```

**Fix `AgenticLlm::complete()` streaming signal:**
The current `AgenticLlm` impl on `AgentLlmClient` emits `is_final=true` after every `complete()` call. In a tool loop with multiple iterations, this prematurely finalizes the chat response. Fix: remove `is_final=true` emission from `complete()` — the caller (`run_tool_loop` / chat event loop) is responsible for signaling completion. Text chunks are emitted with `is_final=false`; the chat event loop emits the final marker after `run_tool_loop` returns.

### API Design

No new public APIs. Changes to existing:

1. **`run_tool_loop`** — takes `messages: Vec<Message>` instead of `initial_message: &str`; gains optional `event_tx` for tool events
2. **`App`** — gains `canonical_messages: Vec<Message>` field
3. **`AgentLlmClient::complete()`** — remove premature `is_final=true` emission

### Implementation Plan

#### Phase 1: Extend `run_tool_loop` to accept prior messages

- Change `initial_message: &str` to `messages: Vec<Message>`
- If messages is empty, the caller is responsible for adding the initial user message
- Update the 3 existing test call sites
- No functional change to callers that pass a single user message

#### Phase 2: Add tool events to the agentic loop

- Add optional `event_tx: Option<broadcast::Sender<DaemonEvent>>` parameter to `run_tool_loop`
- Emit existing `AgentEvent::ToolStarted` before each tool execution
- Emit existing `AgentEvent::ToolCompleted` after each tool execution (with duration and error status)
- Chat tab's event loop picks these up and pushes `ChatMessage::tool_invocation()` to display history
- Fix `AgentLlmClient::complete()` to NOT emit `is_final=true` — only emit text chunks with `is_final=false`

#### Phase 3: Wire Chat tab to use `run_tool_loop`

- Add `canonical_messages: Vec<Message>` to `App`
- Create `ToolExecutor::chat(&[])` and `ToolContext` at TUI startup
- On chat submit:
  - Append user message to `canonical_messages`
  - Clone `canonical_messages` for the spawned task
  - Call `run_tool_loop(llm, executor, ctx, system_prompt, messages, max_iters, event_tx)`
  - On completion, send back `AgenticResult` via a oneshot channel
  - Event loop receives result: replace `canonical_messages` with `result.messages`
  - Emit final `is_final=true` chunk to finalize the streaming display
  - Push `ChatMessage::assistant(result.text)` to display history
- On `/clear`: reset both `chat_history` and `canonical_messages`

#### Phase 4: Handle streaming display during tool loop

- Text chunks from `AgenticLlm::complete()` already emit via broadcast channel
- Tool invocation events are picked up and rendered inline
- Between tool loop iterations, the "streaming" indicator stays active
- On final `AgenticResult`, emit the final chunk marker

## Alternatives Considered

### Alternative A: Embed history in initial message

- **Description:** Serialize prior conversation into the system prompt or initial message each turn.
- **Pros:** No changes to `run_tool_loop` signature.
- **Cons:** Wastes tokens duplicating context. Loses structured tool_use/tool_result blocks (LLM sees them as text, not native blocks). Can't "re-read that file" since the model doesn't see its prior tool calls as tool calls.
- **Why not chosen:** Inferior context quality, token waste, breaks tool continuity.

### Alternative B: Add tools to `call_with_history` (no loop)

- **Description:** Add tool definitions to the existing streaming call but don't execute them — just show the LLM's tool call requests to the user.
- **Pros:** Minimal change.
- **Cons:** Tools don't actually execute. User would have to manually run commands. Defeats the purpose.
- **Why not chosen:** Doesn't solve the problem.

### Alternative C: Separate streaming loop (not `run_tool_loop`)

- **Description:** Build a chat-specific agentic loop in `tui/run.rs` that handles streaming SSE + tool execution inline.
- **Pros:** Full streaming during tool use turns.
- **Cons:** Duplicates the agentic loop logic. Two implementations to maintain. The non-streaming approach is acceptable for now; streaming tool-use can be added to `AgenticLlm::complete()` later without changing the loop.
- **Why not chosen:** Unnecessary duplication. Non-streaming is fine for MVP; upgrade path is clear.

## Technical Considerations

### Dependencies

- No new external dependencies
- Internal: `tools::agentic_loop`, `tools::executor`, `tools::context`, `tools::types`, `agents::llm_client`

### Performance

- Non-streaming `AgenticLlm::complete()` means no token-by-token display during individual LLM calls within the tool loop. Text appears all at once per loop iteration. This is the same behavior as Claude Code when it's executing tools.
- Conversation history grows with tool_use/tool_result blocks, increasing token usage over long sessions. Token budget management is a non-goal for this change but will be needed later.

### Security

- `ToolContext` sandbox enforcement prevents tools from accessing files outside the working directory
- Default deny patterns block `.env`, `.key`, `.pem`, credentials files
- Shell tool respects sandbox boundaries
- Chat uses `ToolExecutor::chat()` which excludes the `plan` tool (agent-only)

### Testing Strategy

- Unit tests for extended `run_tool_loop` with prior messages
- Unit test for `App.canonical_messages` lifecycle (append, clear)
- Integration test: mock LLM returns tool_use → verify tool execution → verify result fed back
- Existing agentic loop tests updated for new signature

### Rollout Plan

- All changes on the `v3` branch
- Phases 1-2 are safe (extend existing APIs with backward-compatible changes)
- Phase 3 replaces the chat LLM call path — test manually with real API key
- Phase 4 is polish — ensure streaming display works

## Edge Cases

### Error handling
If `run_tool_loop` returns `Err`, the event loop must:
1. Push `ChatMessage::system("Error: {err}")` to display history
2. Set `chat_streaming = false`
3. Do NOT update `canonical_messages` (preserve conversation state for retry)

### `/clear` during streaming
Currently `/clear` is blocked while streaming (input is noop). This is acceptable. If we want to support cancellation later, the spawned `JoinHandle` can be aborted.

### Empty tool loop result
If the LLM hits `max_iterations` with only tool calls and no final text, `result.text` will be empty. Push a system message: "Tool loop reached maximum iterations without a final response."

### `/draft` and `/plan` with tools
The `/draft` command generates a structured plan and benefits from codebase access. It should also use the tool loop. The system prompt already changes per `FunnelState` — this is orthogonal to tool availability. All chat states get tools.

### Working directory
Use `std::env::current_dir()` at TUI startup. This matches Claude Code behavior — the user's CWD is the project root. Sandbox enforcement keeps tools within this directory.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Non-streaming feels sluggish for long LLM responses | Medium | Low | Text is emitted as a single chunk after each LLM call; tool invocations show progress. Can add streaming later. |
| Conversation history grows unbounded | Medium | Medium | Non-goal for now. Future: summarize/truncate old messages. |
| Tool sandbox escape via crafted LLM output | Low | High | ToolContext validates all paths. Shell tool is sandboxed. Deny patterns block sensitive files. |
| Breaking existing agent tool system | Low | High | Agents don't use `run_tool_loop` yet (they use action parsing). This change only affects TUI chat. |
| Tool loop error leaves chat in broken state | Medium | Medium | Error path resets `chat_streaming` and preserves `canonical_messages`. User can retry. |

## Open Questions

- [ ] Max iterations for chat tool loop — use `AgentRoleConfig.max_iterations` (default 10) or a separate chat-specific limit?
- [ ] Should tool output (file contents, grep results) be shown in the chat, or just the tool invocation indicator? (Claude Code shows full output; we could do the same with a collapsible view later.)

## References

- Existing design docs: `docs/design/2026-02-26-implementer-reviewer-agents.md` (tool system), `docs/design/2026-03-04-native-tool-use.md` (unified tool design)
- `src/tools/agentic_loop.rs` — agentic loop implementation
- `src/tui/run.rs` — current chat flow
- `src/agents/llm_client.rs` — LLM client with new `AgenticLlm` impl
