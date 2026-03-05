# Design Document: Daemon-Owned Chat

**Author:** Scott Idler + Claude
**Date:** 2026-03-05
**Status:** Implemented
**Review Passes Completed:** 5/5 + 4/5 enhancement (neuraphage learnings)

## Summary

The TUI Chat currently runs its agentic loop (`run_tool_loop`) as a local Tokio task inside the TUI process. This means chat history lives in `App::canonical_messages` (volatile memory), the loop dies if the TUI exits, and Chat operates outside the daemon's unified agent infrastructure. This design promotes Chat to a first-class `AgentSession` managed by the daemon — the same infrastructure that already runs Implementer, Reviewer, Coordinator, and Researcher agents. The TUI becomes a thin rendering client that sends user messages over IPC and passively renders streamed `agent.llm_output` and `agent.tool_*` events. This gives Chat durability (survives TUI restarts), unified execution (daemon-managed `ToolExecutor` and `run_tool_loop`), and positions it to inherit context window management (delegate tool, auto-compaction) without separate wiring. Three additional enhancements — drawn from neuraphage's battle-tested architecture — harden the design: per-iteration checkpointing (so cancellation and crashes lose at most one LLM round-trip, not the whole run), `chat.attach` as a first-class IPC operation (explicit subscription to a running session's event stream), and double-fork daemonization (proper Unix daemon lifecycle so the daemon survives terminal session closure).

## Problem Statement

### Background

Loopr's daemon manages all background agents via `AgentSession` records persisted in TaskStore, with Tokio tasks running `run_tool_loop` server-side. Events stream to all connected TUI clients over the Unix socket broadcast channel. This architecture is proven across four agent types (Implementer, Reviewer, Coordinator, Researcher) and one deterministic task (Integrator).

Chat is the exception. In `src/tui/run.rs:289-300`, when the user submits a message, the TUI spawns a local `tokio::spawn(run_tool_loop(...))` task. The conversation history (`canonical_messages`) lives in `App` struct memory. The LLM client (`AgentLlmClient`) is created TUI-side with a local broadcast channel (`llm_event_tx`) that never touches the daemon.

### Problem

1. **No durability.** If the TUI exits (Ctrl+C, terminal crash, laptop close), the chat history and any in-progress LLM/tool execution are destroyed. There is no way to resume.

2. **Two execution environments.** Background agents run daemon-side with the daemon's working directory and event broadcasting. TUI Chat uses a separate `ToolExecutor::chat(&[])` created at TUI startup with `std::env::current_dir()`. The same `run_tool_loop` code runs in two processes with different tool executor instances, different working directories, and different event channels.

3. **No observability from the daemon.** The daemon has no record that a Chat session exists. There's no `AgentSession` for Chat, no events in `agent_events`, no status in `agent.list`. Chat is invisible to the daemon's management layer.

4. **Context window management must be wired twice.** The upcoming delegate tool and auto-compaction (per the context window management design doc) operate inside `run_tool_loop`. If Chat runs TUI-side and agents run daemon-side, the context management features must be constructed and configured in two places. Moving Chat to the daemon means a single integration point.

5. **No backgrounding.** The user cannot fire off a heavy Chat request, switch to the Dashboard view, and come back later. The TUI's event loop is coupled to the LLM task's lifecycle.

### Goals

- Chat operates as a daemon-managed `AgentSession` with `agent_type: Chat`
- Chat history (`canonical_messages`) persists in TaskStore, surviving TUI restarts
- TUI renders Chat by consuming `agent.llm_output` and `agent.tool_*` events from the daemon broadcast channel — same path as Agent View
- User submits messages via IPC RPC (`chat.submit`), not local task spawn
- TUI can reconnect to a running Chat session and rehydrate the full conversation
- Existing `agent.stop` / `agent.pause` / `agent.resume` IPC methods work for Chat sessions
- Chat inherits context window management (delegate, compaction) from the single daemon-side `run_tool_loop` integration
- Chat uses `ToolExecutor::chat()` (same tool set as current TUI Chat — read, grep, shell, write, edit, glob) not `ToolExecutor::standard()` (which is for background agents with different tool sets)
- Per-iteration checkpointing: conversation state persisted after every completed tool loop iteration, so cancellation or crash loses at most one LLM round-trip
- `chat.attach` as a first-class IPC operation: TUI explicitly subscribes to a running Chat session's event stream, making the client-session relationship explicit rather than implicit broadcast filtering
- Daemon uses proper double-fork daemonization so it is fully detached from the spawning terminal session

### Non-Goals

- Fine-grained mid-request CancellationToken plumbed into reqwest/child processes (coarse-grained JoinHandle abort is sufficient for MVP; mid-LLM/mid-tool cancellation is a fast-follow)
- Multi-session Chat (multiple simultaneous conversations) — single "default-chat" session for now
- Funnel state machine changes (Chat/Interview/PlanDraft/Executing states remain; they just drive TUI rendering, not execution location)
- Reverse control flow for user confirmation prompts (daemon asking TUI for input) — future work
- Streaming individual tool stdout over IPC — future polish

## Proposed Solution

### Overview

Six changes, all additive to existing infrastructure:

1. **New `Chat` agent type** — Add `Chat` variant to `AgentType` enum. Chat sessions are `AgentSession` records with `agent_type: Chat`, managed by the daemon like any other agent.

2. **New IPC methods** — `chat.submit` (send user message, start/resume loop), `chat.attach` (subscribe to running session's event stream + rehydrate history), `chat.history` (fetch full conversation without attaching). All route through the existing `dispatch()` handler.

3. **TUI becomes a renderer** — Remove `run_tool_loop` from `src/tui/run.rs`. The TUI sends `chat.submit` over IPC and renders events from the daemon broadcast channel. The `canonical_messages` field moves from `App` to the daemon's `ChatSession` state.

4. **Per-iteration checkpointing** — The Chat task wrapper persists `ChatHistory` to TaskStore after every completed iteration of `run_tool_loop`, not just on final completion. If the task is aborted or the daemon crashes, at most one LLM round-trip of work is lost.

5. **`chat.attach` as first-class operation** — Instead of the TUI implicitly filtering the global broadcast channel by session ID, the TUI explicitly attaches to a Chat session. The daemon registers the client as an attached observer and can scope event delivery. On attach, the daemon returns full conversation history + current status — combining rehydration and subscription in a single atomic operation.

6. **Double-fork daemonization** — Replace `std::process::Command::spawn()` with proper Unix double-fork (fork → setsid → fork) so the daemon is fully detached from the spawning terminal session. The daemon survives terminal closure, SSH disconnection, and shell exit.

### Architecture

```
Before (current):
┌─────────────────────────────────────┐
│                 TUI                  │
│  App.canonical_messages (volatile)   │
│  tokio::spawn(run_tool_loop(...))    │──── LLM API ────→ Anthropic
│  local AgentLlmClient               │
│  local ToolExecutor::chat()          │
└─────────────────────────────────────┘

After (proposed):
┌─────────────────────────────────────┐
│                 TUI                  │
│  Renders events from broadcast       │
│  Sends chat.submit / chat.history    │
│  No LLM client, no ToolExecutor     │
└──────────────┬──────────────────────┘
               │ IPC (Unix socket, NDJSON)
┌──────────────▼──────────────────────┐
│               Daemon                 │
│  ChatSession in AgentSession store   │
│  run_tool_loop (same as all agents)  │──── LLM API ────→ Anthropic
│  AgentLlmClient (daemon-side)        │
│  ToolExecutor (daemon-side)          │
│  broadcast: agent.llm_output, etc.   │
│  TaskStore: persisted messages       │
└─────────────────────────────────────┘
```

### Data Model

#### AgentType Extension

```rust
pub enum AgentType {
    Implementer,
    Reviewer,
    Coordinator,
    Researcher,
    Integrator,
    Chat,  // NEW
}
```

#### Chat Message Persistence

The `AgentSession` struct does not need a `messages: Vec<Message>` field. Instead, Chat messages are persisted separately — as a JSONL record in TaskStore keyed by session ID, or as a dedicated file in `session_dir`.

**Option A: TaskStore ChatHistory record (preferred)**

```rust
/// Persisted chat conversation. One record per chat session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatHistory {
    pub session_id: String,
    pub messages: Vec<Message>,
    pub funnel_state: FunnelState,
    pub updated_at: i64,
}
```

Stored in TaskStore as collection `"chat_history"`. The daemon loads it on startup and writes it after each `run_tool_loop` completion.

**Option B: Session-dir file**

Write `{session_dir}/chat-{session_id}.jsonl` — one JSON line per message. Simpler but doesn't benefit from TaskStore's SQLite cache.

Option A is preferred because it uses the existing TaskStore infrastructure, supports querying, and integrates with the session diagnostics system.

#### Daemon In-Memory State

Add to `Stores`:

```rust
pub struct Stores {
    // ... existing fields ...
    /// Chat session conversation history (loaded from TaskStore on startup).
    pub chat_sessions: StdRwLock<HashMap<String, ChatHistory>>,
}
```

### API Design

#### `chat.submit` — Send User Message

**Request:**
```json
{
    "id": 1,
    "method": "chat.submit",
    "params": {
        "session_id": "default-chat",
        "message": "read all the .rs files in src/agents/ and summarize them",
        "funnel_state": "Chat"
    }
}
```

**Response:**
```json
{
    "id": 1,
    "result": {
        "session_id": "default-chat",
        "status": "Running"
    }
}
```

**Daemon behavior:**
1. Look up or lazy-create `ChatHistory` for `session_id`
2. Append user `Message` to `chat_history.messages`
3. Look up or lazy-create `AgentSession` with `agent_type: Chat`
4. If session status is `Idle`: spawn `run_tool_loop` Tokio task, transition to `Running`
5. If session status is `Running`: return error (loop already active — wait for completion or cancel)
6. Persist `ChatHistory` to TaskStore
7. Return session status

The spawned task:
- Runs `run_tool_loop(llm, executor, ctx, system_prompt, messages, max_iterations, Some(&event_tx))`
- On completion: updates `ChatHistory.messages` from `AgenticResult.messages`, persists to TaskStore, transitions session to `Idle`
- On error: transitions session to `Idle` (not `Failed` — Chat sessions are long-lived, errors are recoverable), persists error message as a system `ChatMessage` in history
- Events stream via the existing daemon broadcast channel — TUI receives them like any other agent's events

#### `chat.attach` — Subscribe to Session (Primary TUI Entry Point)

**Request:**
```json
{
    "id": 2,
    "method": "chat.attach",
    "params": {
        "session_id": "default-chat"
    }
}
```

**Response:**
```json
{
    "id": 2,
    "result": {
        "session_id": "default-chat",
        "status": "Idle",
        "funnel_state": "Chat",
        "messages": [ ... ],
        "streaming": false
    }
}
```

**Daemon behavior:**
1. Look up or lazy-create `ChatHistory` + `AgentSession` for `session_id`
2. Register this IPC client as an attached observer of the session
3. Return full message array + current status + whether the loop is actively streaming

**Why `attach` instead of just `chat.history`:** This is modeled after neuraphage's `AttachTask` pattern. Attach is an atomic operation that combines rehydration (get history) with subscription (receive future events). The daemon knows which clients are attached to which sessions. This enables:
- **Scoped event delivery** (future): instead of broadcasting all events to all clients and relying on client-side filtering, the daemon can send Chat events only to attached clients
- **Detach detection**: the daemon can detect when no clients are attached and potentially pause expensive operations (future optimization)
- **Multi-session support** (future): when multiple Chat sessions exist, each TUI attaches to one specific session

**TUI behavior on connect:**
1. Call `chat.attach("default-chat")` — gets history + registers for events
2. Render all historical messages
3. Incoming events for this session are delivered by the daemon
4. If `streaming: true`, set `chat_streaming = true` to render incoming chunks

#### `chat.history` — Read-Only History Fetch

Still available as a lightweight read-only alternative to `chat.attach`. Same request/response format but does NOT register the client as an observer. Useful for CLI commands like `loopr chat-log` that just want to dump history without subscribing to events.

#### Existing Methods That Work (With Minor Adaptation)

- **`agent.stop`** — For Chat sessions, aborts the Tokio task via `JoinHandle::abort()` (stored in `stores.agent_handles`), then transitions session to `Idle`. This is different from background agents where `agent.stop` sets `Cancelled` (terminal) and the agent detects it on next iteration. Chat needs `Idle` (not `Cancelled`) because the user will submit again. Aborting the JoinHandle kills it at the next `.await` point (the `llm.complete()` call or tool execution). Per-iteration checkpointing ensures messages are preserved up to the last completed iteration — at most one LLM round-trip of work is lost.
- **`agent.pause` / `agent.resume`** — Not meaningful for Chat in MVP. Background agents check `session.status == Paused` in their outer loop; `run_tool_loop` doesn't check pause status. Could be added later if Chat needs a "thinking pause" feature.
- **`agent.status`** — Returns Chat session status. Works unchanged.
- **`agent.list`** — Chat sessions appear alongside Implementer/Reviewer/etc. Works unchanged.

### Per-Iteration Checkpointing

**Prior art:** Neuraphage's `SupervisedExecutor` persists execution state (conversation + iteration count + token usage) via a syncer loop that checkpoints every 30 seconds. On crash, tasks resume from the last checkpoint. This design adapts the concept for loopr's Chat sessions.

**Problem solved:** Without checkpointing, if `agent.stop` aborts the JoinHandle or the daemon crashes mid-loop, all messages from the current `run_tool_loop` invocation are lost. The user's question is gone, the LLM's partial work is gone, and the tool results are gone. The user must re-ask from scratch.

**Design:**

The Chat task wrapper (the `tokio::spawn` closure in the `chat.submit` handler) holds a shared reference to the `ChatHistory` in `Stores`. After each completed iteration of the tool loop — specifically, after tool results are appended to messages and before the next `llm.complete()` call — the wrapper checkpoints the conversation:

```rust
// Inside the spawned Chat task (pseudocode)
async fn run_chat_task(
    stores: Arc<Stores>,
    session_id: String,
    llm: Arc<dyn AgenticLlm>,
    executor: Arc<ToolExecutor>,
    ctx: ToolContext,
    system_prompt: String,
    messages: Vec<Message>,
    max_iterations: u32,
    event_tx: broadcast::Sender<DaemonEvent>,
) {
    let tool_defs = executor.definitions();
    let mut messages = messages;

    for iteration in 0..max_iterations {
        // LLM call
        let (blocks, stop_reason) = llm.complete(&system_prompt, &messages, &tool_defs).await?;
        messages.push(assistant_message(blocks.clone()));

        if no_tool_calls(&blocks, stop_reason) {
            break; // Final response — will be persisted below
        }

        // Execute tools
        let results = execute_tools(&executor, &ctx, &blocks, &event_tx).await;
        messages.push(tool_results_message(results));

        // === CHECKPOINT ===
        // Persist after every completed iteration (LLM response + tool results)
        checkpoint_chat(&stores, &session_id, &messages).await;
    }

    // Final persist on completion
    finalize_chat(&stores, &session_id, &messages).await;
}

async fn checkpoint_chat(stores: &Stores, session_id: &str, messages: &[Message]) {
    let mut sessions = stores.chat_sessions.write().unwrap();
    if let Some(history) = sessions.get_mut(session_id) {
        history.messages = messages.to_vec();
        history.updated_at = now_millis();
    }
    // Persist to TaskStore
    if let Some(ref store) = stores.store {
        let store = store.lock().await;
        // upsert ChatHistory record
    }
}
```

**This means `run_tool_loop` cannot be called as a black box for Chat.** The Chat task must run the iteration loop directly (or `run_tool_loop` must accept a checkpoint callback). Two options:

**Option A: Chat runs its own iteration loop (preferred)**

The Chat task duplicates the `run_tool_loop` iteration logic (it's ~50 lines) with checkpoint calls between iterations. This keeps `run_tool_loop` unchanged for other agents.

**Option B: Add checkpoint callback to `run_tool_loop`**

```rust
pub async fn run_tool_loop(
    llm: &dyn AgenticLlm,
    executor: &ToolExecutor,
    ctx: &ToolContext,
    system_prompt: &str,
    messages: Vec<Message>,
    max_iterations: u32,
    event_tx: Option<&broadcast::Sender<DaemonEvent>>,
    on_iteration: Option<Box<dyn Fn(&[Message]) -> BoxFuture<'_, ()> + Send + Sync>>,  // NEW: async checkpoint callback
) -> eyre::Result<AgenticResult>
```

Option B is cleaner long-term (other agents could checkpoint too) but changes `run_tool_loop`'s signature. Option A is safer for MVP.

**Recovery behavior:**
- On `agent.stop` (JoinHandle abort): messages are preserved up to the last checkpoint. The user sees their question and all tool results up to that point. At most one LLM round-trip is lost.
- On daemon crash: TaskStore has the last checkpoint. On restart, `ChatHistory` is loaded with all messages up to the last completed iteration. The session resumes as `Idle`.
- On `chat.submit` after abort/crash: the daemon loads the checkpointed messages and continues the conversation from where it left off.

### System Prompt Construction

The TUI currently selects system prompts based on `FunnelState` (Chat, Interview, PlanDraft, Executing). This logic moves to the daemon handler for `chat.submit`:

```rust
fn system_prompt_for_chat(funnel_state: FunnelState) -> String {
    match funnel_state {
        FunnelState::Chat => CHAT_SYSTEM_PROMPT.to_string(),
        FunnelState::Interview => format!("{CHAT_SYSTEM_PROMPT}\n\n{INTERVIEW_PROMPT}"),
        FunnelState::PlanDraft => format!("{CHAT_SYSTEM_PROMPT}\n\n{DRAFT_PROMPT}"),
        FunnelState::Executing => CHAT_SYSTEM_PROMPT.to_string(),
    }
}
```

The `funnel_state` is passed in `chat.submit` params so the daemon knows which prompt variant to use. The TUI still owns the funnel state machine (Chat → Interview → PlanDraft → Executing transitions driven by user input like `/plan`, `/draft`, `/accept`).

**What moves to the daemon:** The system prompt constants (`CHAT_SYSTEM_PROMPT`, `INTERVIEW_PROMPT`, `DRAFT_PROMPT`, `PLAN_REFINE_PROMPT`) and `system_prompt_for_chat()` move from `src/tui/run.rs` to a shared location (e.g., `src/agents/chat.rs` or `src/domain/chat.rs`). The TUI no longer needs them.

### Session Lifecycle

```
TUI opens → chat.attach("default-chat")
  → Not found: daemon lazy-creates ChatHistory + AgentSession (status: Idle)
  → Found: return existing messages + status
  → Client registered as attached observer

User types message → chat.submit("default-chat", "message", funnel_state)
  → Daemon appends message, spawns chat task (iteration loop with checkpointing)
  → Session status: Idle → Running
  → Events stream to attached clients: agent.llm_output, agent.tool_started, agent.tool_completed
  → After each iteration: checkpoint messages to TaskStore
  → Loop completes: final persist, status → Idle

User types another message → chat.submit("default-chat", "next message", funnel_state)
  → Daemon appends, spawns new chat task with full checkpointed history
  → Same cycle (Idle → Running → Idle)

User closes TUI → daemon detects client disconnect
  → Chat session remains in TaskStore (checkpointed)
  → If loop was running, it continues to completion (Running → Idle)
  → Events are still emitted but no attached clients receive them

User reopens TUI → chat.attach("default-chat")
  → Full conversation restored from last checkpoint
  → Client re-registered as attached observer
  → If loop is still running, TUI sees streaming=true and renders incoming events

User hits Ctrl+C during generation → agent.stop(session_id)
  → Daemon aborts JoinHandle, session → Idle
  → Messages preserved up to last checkpoint (at most one LLM round-trip lost)
  → User can immediately submit next message

Daemon crashes mid-loop
  → TaskStore has last checkpoint (persisted after every completed iteration)
  → On restart: ChatHistory loaded, session status → Idle
  → User attaches and sees conversation up to last checkpoint
```

### AgentStatus for Chat

The existing `AgentStatus` enum needs a small adjustment. Currently, `Completed` is terminal — no further transitions. But Chat sessions need to go `Completed → Running` when the user submits another message.

**Option A: New `Idle` status (preferred)**

Add `Idle` to `AgentStatus`. Chat sessions transition: `Starting → Running → Idle` (loop done, awaiting next message). `Idle → Running` on next `chat.submit`. `Idle` is not terminal.

```rust
pub enum AgentStatus {
    Starting,
    Running,
    WaitingForLlm,
    Paused,
    Idle,       // NEW: loop completed, awaiting next input (Chat only)
    Completed,  // terminal
    Failed,     // terminal
    Cancelled,  // terminal
}
```

**Option B: Reuse Completed as non-terminal for Chat**

Allow `Completed → Running` transition only for `agent_type: Chat`. Muddies the semantics.

Option A is cleaner. `Idle` clearly means "loop done, session alive." Chat sessions use only two active states: `Idle` and `Running`. They never enter `Completed`, `Failed`, or `Cancelled` (which are terminal for background agents). Errors and cancellations transition Chat back to `Idle` because Chat is inherently a long-lived, multi-turn session.

### Double-Fork Daemonization

**Prior art:** Neuraphage uses the `fork` crate to perform proper Unix double-fork daemonization. Loopr and taskdaemon both use `std::process::Command::spawn()`, which creates a child process but does NOT fully detach it from the terminal session.

**Problem solved:** With `Command::spawn()`, the daemon is a child of the spawning shell. If the terminal session ends (SSH disconnect, terminal emulator close, `exit`), the daemon may receive SIGHUP and die — depending on the shell's `huponexit` setting and whether the process has called `setsid()`. This is unreliable. For a daemon that should run indefinitely and survive everything short of `kill -9` or system reboot, proper daemonization is required.

**Design:**

Replace the current `ensure_daemon()` in `src/daemon/mod.rs` with double-fork:

```rust
use fork::{Fork, daemon};

pub fn ensure_daemon(config: &Config) -> eyre::Result<()> {
    let pid_path = &config.daemon.pid_path;
    let socket_path = &config.daemon.socket_path;

    // ... existing PID check logic (unchanged) ...

    // No live daemon — daemonize
    eprintln!("Starting daemon...");

    // Double-fork: parent returns immediately, grandchild becomes daemon
    // CRITICAL: this must happen BEFORE any Tokio runtime is created.
    // The fork crate's daemon() does: fork → setsid → fork → redirect stdio.
    match daemon(false, false) {
        // false, false = don't chdir to /, don't keep stdout/stderr
        Ok(Fork::Parent(_)) => {
            // Parent: wait for socket to appear, then return to caller
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if socket_path.exists() {
                    return Ok(());
                }
            }
            Err(eyre::eyre!("daemon started but socket never appeared"))
        }
        Ok(Fork::Child) => {
            // Grandchild: this IS the daemon process.
            // Create a fresh Tokio runtime (no runtime existed pre-fork).
            // This code path never returns to the TUI caller.
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let session_id = generate_session_id();
                let log_path = setup_logging(config, None, Some(&session_id))?;
                let session_dir = log_path.parent().map(PathBuf::from).unwrap_or_default();
                let (ctx, _) = DaemonContext::shared(config.clone(), session_id, session_dir)?;
                daemon_main(ctx).await
            })
        }
        Err(e) => Err(eyre::eyre!("fork failed: {}", e)),
    }
}
```

**Key properties of double-fork:**
1. **First fork** — creates child, parent exits. Child is no longer the session leader.
2. **`setsid()`** — child becomes session leader of a new session, detached from the original terminal.
3. **Second fork** — grandchild is NOT the session leader, so it can never accidentally acquire a controlling terminal.
4. **Redirect stdin/stdout/stderr to /dev/null** — daemon doesn't hold the terminal's file descriptors.

**Critical Tokio constraint:** The Tokio runtime must be created AFTER the fork, not before. Forking a process with an active Tokio runtime corrupts the runtime's internal state (epoll file descriptors, thread pool, timers). Neuraphage handles this by forking in `main()` before `#[tokio::main]` runs.

**Loopr's problem:** Currently `main()` uses `#[tokio::main]`, so the Tokio runtime is already active when `ensure_daemon()` is called at line 37 of `src/main.rs`. We cannot fork from inside `#[tokio::main]`.

**Solution:** Split `main()` into a sync entry point that handles daemonization BEFORE Tokio:

```rust
// No #[tokio::main] — sync entry point
fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli_args = Cli::parse();
    let config = Config::load(cli_args.config.as_ref())?;

    match cli_args.command {
        Some(Command::Tui) | None => {
            // ensure_daemon may fork — must happen before Tokio
            ensure_daemon(&config)?;
            // NOW create the runtime for the TUI
            tokio::runtime::Runtime::new()?.block_on(async {
                run_tui(&config.daemon.socket_path).await
            })
        }
        Some(Command::Daemon) => {
            // Explicit foreground daemon — create runtime directly
            tokio::runtime::Runtime::new()?.block_on(async {
                daemon_main(ctx).await
            })
        }
        // ... other commands create runtime similarly
    }
}
```

If `ensure_daemon` determines a daemon is already running, it returns immediately and the Tokio runtime is created for the TUI. If it needs to spawn a daemon, it forks — the parent returns (and creates the TUI runtime), the grandchild creates its own daemon runtime.

**New dependency:** `fork` crate (pure Rust, no external dependencies, well-maintained). Add via `cargo add fork`.

**Backward compatibility:** The `loopr daemon` subcommand (explicit foreground daemon start) is unchanged. Double-fork only applies to the auto-spawn path in `ensure_daemon()`.

### What Gets Removed from the TUI

After this change, the following are removed from `src/tui/run.rs`:

1. `AgentLlmClient` creation (lines 80-88) — no more TUI-side LLM client
2. `ToolExecutor::chat()` creation (line 92) — no more TUI-side tool executor
3. `ToolContext` creation (line 93) — no more TUI-side tool context
4. The `tokio::spawn(run_tool_loop(...))` block (lines 289-300) — replaced by `chat.submit` IPC call
5. The `llm_task` tracking and completion handling (lines 366-398) — replaced by event-driven rendering
6. The `llm_event_tx` / `llm_event_rx` local broadcast channel — events come from daemon's broadcast channel

The TUI retains:
- `chat_history: Vec<ChatMessage>` — display-only rendering state (populated from `chat.history` on connect + live events)
- `chat_input`, `chat_cursor_pos` — input state
- `chat_streaming: bool` — whether to show typing indicator
- `chat_response_buffer: String` — accumulates streamed chunks for display
- `pending_chat_submit` — triggers `chat.submit` IPC call instead of local task spawn
- `funnel_state` — drives prompt selection and UX chrome
- `canonical_messages` is **removed** — daemon owns the conversation history

### Implementation Plan

**Phase 1: Double-fork daemonization**
Files: `src/main.rs`, `src/daemon/mod.rs`, `Cargo.toml`
- `cargo add fork`
- Restructure `main()`: remove `#[tokio::main]`, create Tokio runtime explicitly after the fork decision point
- Replace `Command::spawn()` in `ensure_daemon()` with double-fork using the `fork` crate
- Grandchild (daemon) creates its own Tokio runtime post-fork
- Parent (TUI/CLI) creates its own Tokio runtime after `ensure_daemon` returns
- PID file written by the grandchild process
- `loopr daemon` subcommand remains foreground (unchanged, creates runtime directly)
- Tests: daemon survives parent exit, PID file written correctly, socket appears

**Phase 2: Data model + IPC skeleton**
Files: `src/agents/mod.rs`, `src/ipc/protocol.rs`, `src/daemon/handlers.rs`, `src/daemon/context.rs`
- Add `Chat` to `AgentType`
- Add `Idle` to `AgentStatus` with transition rules
- Add `ChatHistory` struct in new `src/domain/chat.rs` (shared between daemon and any future consumers)
- Move `FunnelState` from `src/tui/app.rs` to `src/domain/chat.rs`, add `Serialize`/`Deserialize`
- Move system prompt constants from `src/tui/run.rs` to `src/domain/chat.rs`
- Add `chat_sessions` to `Stores`
- Register `chat.submit`, `chat.attach`, and `chat.history` handler stubs in `dispatch()`
- Tests for new types and transitions

**Phase 3: Daemon-side Chat execution with checkpointing**
Files: `src/daemon/handlers.rs`, `src/daemon/context.rs`
- Implement `chat.submit` handler: lazy-create session, append message, spawn chat task
- Implement chat task with per-iteration checkpointing (persist `ChatHistory` after every completed tool iteration)
- Implement `chat.attach` handler: return messages + status, register client as observer
- Implement `chat.history` handler: read-only history fetch
- Create daemon-side `AgentLlmClient` and `ToolExecutor` for Chat sessions
- Load `ChatHistory` from TaskStore on daemon startup
- Tests: submit → checkpoint after each iteration → abort → verify checkpoint preserved

**Phase 4: TUI migration**
Files: `src/tui/run.rs`, `src/tui/app.rs`
- Remove TUI-side LLM client, tool executor, tool context
- Remove local `tokio::spawn(run_tool_loop(...))` task
- Replace `pending_chat_submit` handling with `chat.submit` IPC call
- Replace `llm_task` completion handling with event-driven rendering
- Implement `chat.attach` call on TUI connect for rehydration + subscription
- Remove `canonical_messages` from `App`
- Tests: verify TUI renders events correctly, verify rehydration via attach

**Phase 5: Session management**
Files: `src/daemon/handlers.rs`
- Verify `agent.stop` works for Chat sessions (abort + checkpoint preservation)
- Handle edge cases: submit while running (reject), submit while paused (reject)
- Test the full lifecycle: attach → submit → stream → checkpoint → complete → resubmit → cancel → reattach
- Test crash recovery: kill daemon mid-loop → restart → attach → verify checkpoint survived

**Phase 6: Cleanup + observability**
Files: various
- Chat sessions appear in `agent.list` output
- Chat sessions appear in TUI Agent View
- Remove dead TUI-side imports (`AgentLlmClient`, etc.)
- Session diagnostics include Chat sessions (checkpoint count, last checkpoint time)
- Verify `otto ci` passes

## Alternatives Considered

### Alternative 1: Keep Chat TUI-Side, Add Persistence Only

- **Description:** Keep `run_tool_loop` in the TUI process but add persistence by writing `canonical_messages` to a file on each completion.
- **Pros:** Smaller change. No IPC additions.
- **Cons:** Two execution environments remain. Context window management must be wired in two places. No backgrounding. If TUI exits mid-loop, the in-progress work is still lost. Doesn't unify the agent model.
- **Why not chosen:** Solves the persistence problem but not the architectural fragmentation. We'd still maintain two `ToolExecutor` instances, two LLM client instances, and two event streaming paths.

### Alternative 2: Full AgentSession Reuse (No ChatHistory Record)

- **Description:** Store chat messages directly in `AgentSession.messages` field instead of a separate `ChatHistory` record.
- **Pros:** Simpler — one record type.
- **Cons:** `AgentSession` is a generic record used by all agent types. Adding a `Vec<Message>` field bloats it for Implementer/Reviewer/Coordinator sessions that don't need it. Messages can be large (hundreds of KB after compaction); mixing them into the session record makes `agent.list` and `agent.status` responses huge.
- **Why not chosen:** Separation of concerns. `AgentSession` tracks lifecycle metadata. `ChatHistory` tracks conversation content. They reference each other by `session_id`.

### Alternative 3: WebSocket / Bidirectional Streaming Protocol

- **Description:** Replace NDJSON over Unix socket with a WebSocket-based protocol for richer streaming.
- **Pros:** Native bidirectional streaming. Better for future reverse control flow (daemon asking TUI).
- **Cons:** Massive protocol change. Existing NDJSON + broadcast channel works perfectly for this use case.
- **Why not chosen:** The existing IPC infrastructure handles request/response + event streaming already. No need to change the wire protocol.

## Technical Considerations

### Dependencies

- New external dependency: `fork` crate (for double-fork daemonization)
- `ChatHistory` needs `Record` trait implementation for TaskStore persistence
- `AgentLlmClient` and `ToolExecutor` already exist and are reusable daemon-side
- `FunnelState` needs `Serialize` + `Deserialize` for IPC params — must move from `tui::app` to a shared module (e.g., `domain::chat` or `agents::chat`) so the daemon can deserialize it without depending on TUI code

### Performance

- **Latency:** One additional IPC round-trip per submission (`chat.submit` request → response) vs. direct local task spawn. On a Unix socket, this is ~0.1ms — imperceptible.
- **Streaming:** LLM token streaming uses the existing daemon broadcast channel, same latency as Agent View events. No regression.
- **Persistence:** TaskStore write on each iteration (checkpoint) and on loop completion. JSONL append + SQLite upsert. ~1ms per checkpoint for typical message histories. With `max_iterations=10`, worst case is 10 checkpoints per submission — still <10ms total, negligible vs the seconds spent on LLM calls.
- **Memory:** `ChatHistory.messages` loaded in daemon memory. For a long session (100 exchanges), this is ~1-5 MB. Trivial.

### Security

- Chat `ToolExecutor` on the daemon side inherits the same sandbox settings as other agents. No change in security posture.
- `ANTHROPIC_API_KEY` is already available to the daemon (used by Implementer/Reviewer/Coordinator). No new secret handling.

### Testing Strategy

- **Unit tests:** `ChatHistory` serialization, `AgentType::Chat` transitions, `Idle` status transitions
- **Handler tests:** `chat.submit` → events emitted → `chat.history` returns messages
- **Integration tests:** Full lifecycle — submit → stream → complete → resubmit → cancel → rehydrate
- **TUI tests:** Existing `extract_llm_chunk` and `extract_tool_event` tests continue to pass (same event format)
- **Regression:** `otto ci` gate ensures no existing tests break

### Rollout Plan

Phase 1 (double-fork) is independent and can land anytime — it improves all daemon usage, not just Chat. Phases 2-5 are purely additive. The old TUI-side Chat path works until Phase 4 removes it. Phase 4 is the switchover — can be done in a single commit since the daemon-side Chat is proven by Phase 3 tests.

### Interaction with Context Window Management Design

The context window management design doc (`2026-03-05-agentic-loop-context-window-management.md`) proposes changes to `run_tool_loop`: delegate tool, auto-compaction, per-result caps, read caps. These two designs are complementary:

- **If context window management lands first:** Chat inherits it automatically when moved to daemon-side `run_tool_loop`. No extra wiring.
- **If daemon-owned Chat lands first:** Context window management needs only one integration point (daemon-side `run_tool_loop`) instead of two (daemon + TUI).
- **Recommended order:** Land daemon-owned Chat first (this design), then context window management. This avoids the "wire it in two places" problem entirely.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Rehydration misses events during reconnect | Medium | Low | `chat.history` returns full message array. TUI renders from scratch on connect. Events only matter for live streaming. |
| User submits while loop is running | Medium | Low | `chat.submit` returns error if session is `Running`. TUI shows "waiting for response..." |
| Large ChatHistory bloats TaskStore | Low | Low | Auto-compaction (from context window design) keeps message history bounded. |
| FunnelState desync between TUI and daemon | Low | Medium | `chat.submit` includes `funnel_state` param. Daemon is stateless w.r.t. funnel — TUI owns the state machine. |
| Daemon crash loses in-progress Chat loop | Medium | Low | Per-iteration checkpointing limits loss to at most one LLM round-trip. Double-fork daemonization eliminates accidental terminal-closure deaths. |
| Fork crate compatibility | Low | Medium | `fork` crate is pure Rust, well-maintained, used by neuraphage in production. Linux-only (not an issue — loopr targets Linux). |
| Checkpoint I/O adds latency | Low | Low | ~1ms per checkpoint (JSONL + SQLite upsert), negligible vs seconds-long LLM calls. |
| Breaking change to `AgentStatus` (new `Idle`) | Low | Low | `Idle` is additive. Existing serialized sessions don't use it. Backward compatible. |
| Chat working directory differs from TUI cwd | Low | Low | Daemon uses project root (where `loopr daemon` started). This is actually correct — Chat should see the full project, not a subdirectory. |

## Edge Cases

### Submit During Active Loop
If the user sends `chat.submit` while the loop is still running, the handler returns an error: "Chat loop is active. Wait for completion or cancel with agent.stop." The TUI should grey out the input field while `chat_streaming` is true (already does this).

### Cancellation and Message Persistence
When `agent.stop` aborts the JoinHandle, the `run_tool_loop` task is killed at the next `.await` point. Per-iteration checkpointing ensures that messages up to the last completed iteration are persisted in TaskStore. At most one LLM round-trip of work is lost — the in-flight `llm.complete()` call or the tool execution that was active when the abort arrived. The user's original question and all completed tool results are preserved. On the next `chat.submit`, the daemon loads the checkpointed messages and continues the conversation.

### TUI Disconnects Mid-Stream
The daemon's `run_tool_loop` task is independent of client connections. It continues to completion. Events are broadcast but silently dropped if no clients are subscribed. When the TUI reconnects, `chat.history` returns all messages including the completed response.

### Daemon Restarts / Crashes
`ChatHistory` is checkpointed to TaskStore after every completed iteration. On daemon restart, the session is loaded with `status: Idle` and messages up to the last checkpoint. At most one LLM round-trip is lost. The user attaches and sees the full conversation up to the crash point, then can continue chatting immediately. With double-fork daemonization, unintentional daemon death from terminal closure is eliminated — crashes only happen from bugs, OOM, or explicit `kill`.

### Multiple TUI Clients
Multiple TUI instances can connect and all receive the same broadcast events. All see the same Chat history via `chat.history`. Only one can submit at a time (the running loop check prevents concurrent submits). This is fine for single-user operation.

### Chat Session Reset
Add a future `chat.reset` method to clear history and start fresh. Not needed for MVP — the user can restart the daemon. But worth noting as a follow-up.

### Working Directory
The TUI currently creates `ToolContext` with `std::env::current_dir()`. The daemon's working directory may differ (it's typically the project root where `loopr daemon` was started). Since Chat tools (read, grep, shell) operate relative to the working directory, the daemon should use the project root — which is the correct behavior (same as background agents). If the user launched the TUI from a subdirectory, Chat should still see the full project. The `chat.submit` request does NOT need to pass a working directory; the daemon uses its own cwd (project root).

### Rehydration Message Format
`chat.history` returns `Vec<Message>` in Anthropic API format (with `ToolUse` and `ToolResult` content blocks). The TUI's `chat_history: Vec<ChatMessage>` is a simplified display format (role + text string). On rehydration, the TUI must transform API messages into display messages: extract text from `Text` blocks, format `ToolUse` as "[Tool: read file.rs]", format `ToolResult` as tool output text. This is the same transformation the TUI already does when building `chat_history` from `AgenticResult` — it just needs to be factored into a reusable function.

### Event Delivery via Attach
With `chat.attach`, the daemon knows which clients are observing which sessions. For MVP, the daemon still broadcasts all events to all clients (the existing broadcast channel), and the TUI filters by session ID. But the attach registration lays the groundwork for scoped delivery: a future optimization where the daemon only sends Chat events to clients that have attached to that session. This eliminates the fan-out concern entirely.

### Funnel State Transitions
The funnel state machine (Chat → Interview → PlanDraft → Executing) remains TUI-owned. The daemon doesn't enforce it — it just receives the `funnel_state` with each `chat.submit` to select the right system prompt. If the TUI sends inconsistent funnel states, the daemon doesn't care; it's the TUI's job to manage the UX flow.

## Open Questions

- [ ] Should `ChatHistory` be a TaskStore record (JSONL + SQLite) or a simpler session-dir file? TaskStore is preferred but requires `Record` trait implementation for the `scottidler/taskstore` crate.
- [ ] Should the daemon support multiple named Chat sessions (e.g., per-project)? Single "default-chat" is sufficient for now.
- [ ] Should `chat.submit` accept `max_iterations` as a parameter, or always use a fixed default (10)?
- [ ] When context window management lands, should auto-compaction persist intermediate compaction snapshots for auditability?
- [ ] Should `chat.history` return raw `Vec<Message>` (Anthropic API format) or a TUI-friendly format (e.g., `Vec<ChatMessage>` with role/content only)? Raw format keeps the daemon simple; TUI can transform.

## References

- Current TUI Chat implementation: `src/tui/run.rs:80-300` (LLM client, tool executor, task spawn)
- Current daemon agent infrastructure: `src/daemon/handlers.rs` (agent.start, agent.stop, agent.pause, agent.resume)
- AgentSession model: `src/agents/mod.rs:284-308`
- Agentic loop: `src/tools/agentic_loop.rs`
- Context window management design: `docs/design/2026-03-05-agentic-loop-context-window-management.md`
- IPC protocol: `src/ipc/protocol.rs`
- App state (canonical_messages, etc.): `src/tui/app.rs:220-231`
- Gemini architectural feedback: conversation context (state ownership, interruptibility, observability)
- Neuraphage prior art: `~/repos/neuraphage/neuraphage/src/daemon.rs` (double-fork daemonization, AttachTask, DaemonRequest/Response)
- Neuraphage agentic loop: `~/repos/neuraphage/neuraphage/src/agentic/mod.rs` (per-iteration checkpointing, conversation persistence)
- Neuraphage supervised executor: `~/repos/neuraphage/neuraphage/src/supervised.rs` (watcher/syncer loops, crash recovery)
- taskdaemon daemon: `~/repos/taskdaemon/taskdaemon/td/src/daemon.rs` (DaemonManager, version tracking, SIGTERM/SIGKILL lifecycle)
- Current daemon spawn: `src/daemon/mod.rs:53-75` (Command::spawn — to be replaced with double-fork)
