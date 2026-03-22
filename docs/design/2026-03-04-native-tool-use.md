# Design Document: Native Tool Use for Loopr Agents

**Author:** Scott A. Idler
**Date:** 2026-03-04
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Loopr's agents and TUI chat currently call the Anthropic Messages API as plain text-in/text-out — no tool definitions, no tool execution loop. The LLM literally cannot interact with the filesystem, run commands, or search code. This design adds 14 built-in tools with native Anthropic `tool_use` support, an agentic execution loop, and a `ToolContext` sandbox — bringing loopr to parity with Claude Code's capabilities.

## Problem Statement

### Background

Loopr agents (Implementer, Researcher, Coordinator) currently use a custom "action-based" pattern: the LLM outputs a JSON array of action objects, which the executor parses and dispatches. This works but has significant limitations:

1. **No schema enforcement** — the LLM learns actions from prompt examples, not formal tool schemas
2. **No agentic loop** — a single LLM call returns all actions at once, no iterative tool use
3. **TUI chat is blind** — the chat LLM has zero tool access (screenshot proves this)
4. **Token waste** — action definitions live in the system prompt, consuming context every turn

The `taskdaemon/td` project already solved this with a full tool system (Tool trait, ToolContext, ToolExecutor, Anthropic tool_use integration). We port that architecture to loopr, adapted for loopr's agent model.

### Problem

When a user asks "what files are in this directory?" in loopr's chat, the LLM responds "I don't have direct access to your filesystem." Every agent interaction that needs file I/O, search, or command execution relies on brittle prompt-based action parsing instead of the Anthropic API's native tool_use protocol.

### Goals

- Define 14 built-in tools matching Claude Code's capabilities (adapted for loopr)
- Implement the `Tool` trait, `ToolContext` (sandbox), and `ToolExecutor` (registry/dispatch)
- Extend `AgentLlmClient` to send tool definitions and parse `tool_use` content blocks
- Implement an agentic loop: call LLM → execute tools → send results → repeat until text response
- Wire tool use into TUI chat (immediate value) and agents (gradual migration)

### Non-Goals

- MCP server integration (future work)
- Background shell management (BashOutput/KillShell — not needed for MVP)
- Jupyter/NotebookEdit support
- Replacing the existing action-based agent system in one shot (gradual migration)
- User approval UX for tool execution (trust model — agents run in sandboxed worktrees)

## Proposed Solution

### Overview

Port `taskdaemon/td`'s tool system to loopr with these adaptations:

1. **14 built-in tools** organized under `src/tools/builtin/`
2. **Tool trait** with `name()`, `description()`, `input_schema()`, `execute()`
3. **ToolContext** for worktree sandboxing, read tracking, path validation
4. **ToolExecutor** for tool registry, definition export, and dispatch
5. **Extended LLM client** that sends `tools` in API body and parses `tool_use`/`tool_result` content blocks
6. **Agentic loop** that iterates until `stop_reason: end_turn`

### Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│ Agent / TUI Chat                                                  │
│                                                                   │
│  ┌──────────────┐     ┌──────────────┐     ┌──────────────────┐ │
│  │ LLM Response │────▶│ ToolExecutor │────▶│ ToolContext      │ │
│  │ (tool_use    │     │              │     │ - worktree: Path │ │
│  │  blocks)     │     │ 14 built-in  │     │ - read_files: Set│ │
│  └──────┬───────┘     │ tools        │     │ - sandbox: bool  │ │
│         │             └──────┬───────┘     └──────────────────┘ │
│         │                    │                                    │
│         │                    ▼                                    │
│         │             ┌──────────────┐                           │
│         │             │ ToolResult   │                           │
│         │             │ {content,    │                           │
│         │             │  is_error}   │                           │
│         │             └──────┬───────┘                           │
│         │                    │                                    │
│         ▼                    ▼                                    │
│  ┌─────────────────────────────────┐                            │
│  │ Next LLM call with tool_result  │◀─── loop until end_turn   │
│  │ messages appended               │                            │
│  └─────────────────────────────────┘                            │
└──────────────────────────────────────────────────────────────────┘
```

### Tool Inventory

#### File System (6 tools)

| Tool | Backend | Parameters | Description |
|------|---------|------------|-------------|
| `read` | native (tokio::fs) | `path`, `offset?`, `limit?` | Read file with line numbers. Tracks reads for edit validation. |
| `write` | native (tokio::fs) | `path`, `content` | Write/create file. Creates parent dirs. |
| `edit` | native (tokio::fs) | `path`, `old_string`, `new_string`, `replace_all?` | Exact string replacement. Requires prior `read`. |
| `list` | `eza` | `path?` | List files and directories. |
| `tree` | `eza --tree` | `path?`, `depth?` | Recursive directory tree. |
| `glob` | `glob` crate | `pattern`, `path?` | Find files by glob pattern (e.g., `**/*.rs`). |

#### Search (2 tools)

| Tool | Backend | Parameters | Description |
|------|---------|------------|-------------|
| `grep` | `rg` (ripgrep) | `pattern`, `path?`, `file_pattern?`, `context?` | Search file contents with regex. |
| `find` | `fd` | `pattern`, `path?`, `type?`, `depth?` | Find files/directories by name pattern. |

#### Execution (2 tools)

| Tool | Backend | Parameters | Description |
|------|---------|------------|-------------|
| `shell` | `bash` (default) | `command`, `timeout_ms?` | Execute shell commands in worktree. |
| `slash` | native | `command` | Execute slash commands within the conversation (e.g., `/draft`, `/accept`). |

#### Web (2 tools)

| Tool | Backend | Parameters | Description |
|------|---------|------------|-------------|
| `fetch` | `reqwest` | `url`, `prompt` | Fetch URL and extract information. |
| `search` | web API | `query`, `allowed_domains?`, `blocked_domains?` | Search the web. |

#### Orchestration (2 tools)

| Tool | Backend | Parameters | Description |
|------|---------|------------|-------------|
| `todo` | native | `todos` (array of `{content, status}`) | Structured task tracking. |
| `plan` | native | `plan` (markdown string) | Present/update implementation plan. |

**14 tools total.**

### Data Model

#### Tool Trait

```rust
// src/tools/traits.rs
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> serde_json::Value;
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}
```

#### ToolContext

```rust
// src/tools/context.rs
#[derive(Clone)]
pub struct ToolContext {
    pub worktree: PathBuf,
    pub exec_id: String,
    read_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub sandbox_enabled: bool,
}

impl ToolContext {
    pub fn new(worktree: PathBuf, exec_id: String) -> Self;
    pub async fn track_read(&self, path: &Path);
    pub async fn was_read(&self, path: &Path) -> bool;
    pub async fn clear_reads(&self);
    pub fn validate_path(&self, path: &Path) -> Result<PathBuf>;
}
```

#### ToolExecutor

```rust
// src/tools/executor.rs
pub struct ToolExecutor {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolExecutor {
    pub fn standard() -> Self;  // All 14 tools
    pub fn chat() -> Self;      // Subset for TUI chat (no plan — plan is agent-only)
    pub fn definitions(&self) -> Vec<ToolDefinition>;
    pub async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult;
    pub async fn execute_all(&self, calls: &[ToolCall], ctx: &ToolContext) -> Vec<(String, ToolResult)>;
}
```

#### LLM Types (new)

```rust
// src/tools/types.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Content blocks use `type` field for Anthropic API serialization:
///   {"type": "text", "text": "..."}
///   {"type": "tool_use", "id": "...", "name": "...", "input": {...}}
///   {"type": "tool_result", "tool_use_id": "...", "content": "...", "is_error": false}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}
```

### API Design

#### Extended LLM Client

The existing `LlmClient` trait gets a new method. The old methods remain for backward compatibility during migration:

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    // Existing (unchanged)
    async fn call(&self, system_prompt: &str, user_message: &str) -> Result<String>;
    async fn call_with_history(&self, system_prompt: &str, messages: &[ChatMessage]) -> Result<String>;

    // New: tool-use aware call
    async fn call_with_tools(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<CompletionResponse>;
}

/// A message in the conversation (Anthropic API format).
/// Content blocks are grouped by role — assistant blocks and user blocks alternate.
pub struct Message {
    pub role: Role,  // "user" or "assistant"
    pub content: Vec<ContentBlock>,
}

pub struct CompletionResponse {
    pub content: Vec<ContentBlock>,  // The assistant's response blocks
    pub stop_reason: StopReason,
}
```

**Key constraint:** The Anthropic API requires tool_result blocks to be in a `user` role message. The agentic loop must construct messages correctly:

```
assistant: [Text("Let me check..."), ToolUse{id, name, input}]
user:      [ToolResult{tool_use_id, content, is_error}]
assistant: [Text("The file contains...")]
```

#### Agentic Loop

```rust
// src/tools/agentic_loop.rs
pub async fn run_tool_loop(
    llm: &dyn LlmClient,
    executor: &ToolExecutor,
    ctx: &ToolContext,
    system_prompt: &str,
    initial_messages: Vec<Message>,
    max_iterations: usize,
    event_tx: Option<&broadcast::Sender<DaemonEvent>>,  // For streaming to TUI
) -> Result<String> {
    let tools = executor.definitions();
    let mut messages = initial_messages;

    for _ in 0..max_iterations {
        let response = llm.call_with_tools(system_prompt, &messages, &tools).await?;

        // Append assistant response as an assistant-role message
        messages.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        match response.stop_reason {
            StopReason::EndTurn => {
                return Ok(extract_text(&response.content));
            }
            StopReason::ToolUse => {
                // Execute tool calls sequentially (order may matter for file I/O)
                let tool_calls = extract_tool_calls(&response.content);
                let mut results = Vec::new();
                for call in &tool_calls {
                    // Emit tool invocation event for TUI display
                    if let Some(tx) = event_tx {
                        emit_tool_start(tx, ctx, call);
                    }
                    let result = executor.execute(call, ctx).await;
                    if let Some(tx) = event_tx {
                        emit_tool_done(tx, ctx, call, &result);
                    }
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: call.id.clone(),
                        content: result.content,
                        is_error: result.is_error,
                    });
                }
                // Tool results go in a user-role message (Anthropic API requirement)
                messages.push(Message {
                    role: Role::User,
                    content: results,
                });
            }
            StopReason::MaxTokens => {
                return Err(eyre!("max tokens reached"));
            }
        }
    }

    Err(eyre!("tool loop exceeded max iterations"))
}
```

#### SSE Parsing Extension

The existing `parse_sse_text_delta()` in `llm_client.rs` only handles `text_delta`. We extend SSE parsing to also handle:

- `content_block_start` with `type: "tool_use"` → capture tool id and name
- `content_block_delta` with `type: "input_json_delta"` → accumulate JSON fragments
- `content_block_stop` → parse accumulated JSON as tool input

```rust
enum StreamState {
    Text,
    ToolUse { id: String, name: String, json_buffer: String },
}
```

### Implementation Plan

#### Phase 1: Tool Infrastructure

**Files created:**
- `src/tools/traits.rs` — `Tool` trait, `ToolResult`
- `src/tools/context.rs` — `ToolContext` (sandbox, read tracking)
- `src/tools/types.rs` — `ToolDefinition`, `ToolCall`, `ContentBlock`, `StopReason`
- `src/tools/executor.rs` — `ToolExecutor` (registry, dispatch, definitions)
- `src/tools/error.rs` — `ToolError` enum

**Files modified:**
- `src/tools/mod.rs` — re-export new modules alongside existing `ToolRunner`

**Note on existing `ToolRunner`:** The current `ToolRunner` (subprocess executor for `cargo test`, `npm run lint`, etc.) is kept as-is. It serves a different purpose — running project-specific build/test commands defined in `loopr.yml`. The new `ToolExecutor` handles LLM-facing tools. The `shell` builtin tool can delegate to `ToolRunner` for configured project tools.

#### Phase 2: Built-in Tools

**Files created (one per tool):**
- `src/tools/builtin/mod.rs`
- `src/tools/builtin/read.rs`
- `src/tools/builtin/write.rs`
- `src/tools/builtin/edit.rs`
- `src/tools/builtin/list.rs`
- `src/tools/builtin/tree.rs`
- `src/tools/builtin/glob.rs`
- `src/tools/builtin/grep.rs`
- `src/tools/builtin/find.rs`
- `src/tools/builtin/shell.rs`
- `src/tools/builtin/slash.rs`
- `src/tools/builtin/fetch.rs`
- `src/tools/builtin/search.rs`
- `src/tools/builtin/todo.rs`
- `src/tools/builtin/plan.rs`

#### Phase 3: LLM Client Extension

**Files modified:**
- `src/agents/implementer.rs` — add `call_with_tools()` to `LlmClient` trait
- `src/agents/llm_client.rs` — implement `call_with_tools()` with:
  - Tool definitions in request body
  - Extended SSE parsing for `tool_use` blocks
  - `CompletionResponse` return type

**Files created:**
- `src/tools/agentic_loop.rs` — `run_tool_loop()` function

#### Phase 4: TUI Chat Integration

**Files modified:**
- `src/tui/run.rs` — replace `call_with_history()` with `run_tool_loop()` for chat
- `src/tui/views/chat.rs` — display tool invocations in chat history (optional)
- `src/tui/app.rs` — add tool-related chat message types

#### Phase 5: Agent Migration

**Files modified (per agent, gradual):**
- `src/agents/researcher.rs` — replace custom SearchCode/ReadFile/ListDirectory actions with native tools
- `src/agents/implementer.rs` — replace ReadFile/WriteFile/RunTool actions with native tools
- `src/agents/coordinator.rs` — add tool use for plan/todo orchestration

**Deferred:** Full removal of action-based system (keep as fallback during migration)

## Alternatives Considered

### Alternative 1: Keep Action-Based System, Add Tools to Prompt

- **Description:** Continue using JSON action arrays. Add new action types for missing capabilities.
- **Pros:** No API-level changes needed. Low risk.
- **Cons:** No schema enforcement. Actions consume prompt tokens every turn. No iterative tool use — LLM must predict all actions upfront. Fragile parsing.
- **Why not chosen:** The action-based system is fundamentally limited. Claude's tool_use protocol exists specifically to solve these problems.

### Alternative 2: Use Claude Code as a Subprocess

- **Description:** Shell out to `claude` CLI for tool-use tasks.
- **Pros:** Gets all Claude Code tools for free. No implementation needed.
- **Cons:** No control over tool execution. Can't sandbox to worktrees. Latency. Cost (separate API calls). Can't integrate with loopr's orchestration (locks, bundles, plans).
- **Why not chosen:** Loopr needs fine-grained control over tool execution for its multi-agent coordination model.

### Alternative 3: MCP Server Integration

- **Description:** Implement tools as MCP servers that Claude can connect to.
- **Pros:** Standard protocol. Reusable across tools.
- **Cons:** Overhead of MCP server lifecycle. Still need to define the tools. Adds complexity without immediate benefit for built-in tools.
- **Why not chosen:** MCP is better for external integrations (Jira, Slack, etc.). Built-in tools should be native for performance and simplicity. MCP can be added later.

## Technical Considerations

### Dependencies

**New crate dependencies:**
- `glob` — already in Cargo.toml (used by Researcher)
- `async-trait` — already in Cargo.toml
- No new dependencies required

**External tool dependencies (runtime):**
- `eza` — for `list` and `tree` tools (fallback to `ls`/`find` if not installed)
- `rg` (ripgrep) — for `grep` tool (fallback to native grep)
- `fd` — for `find` tool (fallback to native find)

### Performance

- Tool definitions add ~2K tokens to each API request (one-time per conversation turn)
- Agentic loop adds latency (multiple round-trips) but enables iterative problem-solving
- Tool execution is sequential within an iteration (order matters for file I/O dependencies)
- SSE streaming preserved — user sees text chunks in real-time between tool calls

### Security

- **Sandbox enforcement:** All file paths validated against worktree root. Symlink traversal prevented via canonicalization.
- **Read-before-edit:** Edit tool refuses to modify files that haven't been read first.
- **Command execution:** Shell tool runs in worktree directory. No escape via path manipulation (sandbox validates).
- **No network sandbox:** fetch/search tools can reach any URL. Acceptable for development tool.

### Testing Strategy

- **Unit tests:** Each builtin tool tested with `tempdir` worktrees (port from td)
- **ToolContext tests:** Sandbox violation, read tracking, path normalization
- **ToolExecutor tests:** Registration, dispatch, unknown tool handling
- **SSE parsing tests:** Extend existing tests for tool_use content blocks
- **Integration tests:** End-to-end agentic loop with mock LLM returning tool_use blocks
- **Existing tests:** All current tests must continue passing (backward compatible)

### Rollout Plan

1. **Phase 1-2** (tool infrastructure + builtins) — no behavioral changes, purely additive
2. **Phase 3** (LLM client extension) — new method on trait, existing methods unchanged
3. **Phase 4** (TUI chat) — immediate user-visible improvement, low risk
4. **Phase 5** (agent migration) — gradual, per-agent, with action fallback

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| SSE parsing for tool_use blocks is complex | Medium | High | Port proven parsing from td; extensive tests |
| Tool definitions increase token usage | Low | Low | ~2K tokens per turn is negligible vs context |
| Sandbox escape via symlinks or path tricks | Low | High | Canonicalize paths; test with adversarial inputs |
| Breaking existing agent action system | Medium | High | Keep action system as fallback; migrate gradually |
| eza/rg/fd not installed on some systems | Low | Medium | Detect at startup; fall back to coreutils |
| Agentic loop infinite cycling | Low | Medium | Hard cap on iterations (configurable, default 25) |

## Edge Cases

### TUI Chat vs Agent ToolContext
Two different scoping models:
- **TUI Chat:** `ToolContext` uses the local CWD — which is usually, but not always, a repo checkout. Sandbox prevents escape above CWD. No git assumption.
- **Agents:** Each agent gets an isolated git worktree for parallel work (via loopr's worktree manager). `ToolContext` is scoped to that worktree. This is how multiple agents can write files concurrently without conflicts.

### Streaming During Tool Loop
The agentic loop must preserve real-time text streaming to the TUI. `call_with_tools()` in `AgentLlmClient` still uses SSE streaming internally — text chunks are emitted via the existing `broadcast::Sender<DaemonEvent>` channel. Between tool iterations, the TUI shows tool invocation/result events (via the `event_tx` parameter in `run_tool_loop`). The loop only blocks on tool execution, not on LLM output.

### External Tool Availability
`eza`, `rg`, and `fd` may not be installed. Strategy:
- **At tool registration time:** check `which eza` / `which rg` / `which fd`
- **If missing:** the tool's `execute()` returns `ToolResult::error("eza not found. Install with: cargo install eza")`
- **No silent fallback** — the LLM should know what happened and can suggest installation

### Slash Tool State Mutation
The `slash` tool needs to mutate TUI app state (funnel state, chat mode). Since the tool executor runs in a different async context, `slash` returns a `ToolResult` with a structured directive (e.g., `{"directive": "enter_plan_mode"}`) that the agentic loop interprets and applies to app state. The tool itself doesn't directly mutate state.

### Mock LlmClient Breakage
Adding `call_with_tools()` to the `LlmClient` trait requires updating all mock implementations in tests. Mitigation: provide a default implementation that panics with "not implemented" so existing mocks compile without changes until they're migrated.

## Open Questions

- [ ] Should TUI chat tool execution show tool invocations inline (like Claude Code) or hide them?
- [ ] Should agents use tool_use exclusively or support hybrid (tool_use + action JSON)?
- [ ] What iteration limit for the agentic loop? (td uses 10 for explorers, Claude Code allows ~100+)
- [ ] How should `plan` tool interact with the existing plan/spec/phase domain model?

## References

- `docs/claudecode-tools-reference.md` — Claude Code built-in tools reference
- `taskdaemon/td/docs/tools.md` — td tool system specification
- `taskdaemon/td/src/tools/` — td tool implementation (source to port from)
- `docs/design/2026-02-26-multi-level-rwl.md` — current MVP4 design (action-based agents)
- Anthropic Messages API — tool_use documentation
