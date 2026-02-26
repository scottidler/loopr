# Ralph Wiggum Loop — ONE TASK THEN EXIT

You are in a Ralph Wiggum loop. You have NO MEMORY of previous runs.
Your state persists ONLY in `progress.txt` and the git history.

## CRITICAL RULES

1. **CHECK THE INJECTED CONTEXT BELOW** — progress, validation output, and quality gate results from previous iterations are appended to this prompt automatically. You do NOT need to read progress.txt yourself.
2. **DO ONE LOGICAL UNIT OF WORK** — see "What is one task?" below. Batch similar/mechanical changes together, but don't mix unrelated work.
3. **EXIT IMMEDIATELY** — do not retry failures. Do not loop. Just exit.
4. **If validation failed last iteration, FIX THAT FIRST** — the validation output is injected below. Read it, fix the issue, done.

The bash loop restarts you with fresh context. That's the whole point.
The bash loop runs validation EXTERNALLY — you do NOT run `otto ci`.

---

## Your Workflow

```bash
# 1. Check injected context (appended below this prompt automatically):
#    - Progress from previous iterations
#    - Last validation output (if it failed)
#    - Last quality gate output (if it failed)
#    You already have this — no need to cat files.

# 2. Optionally check current code state
git log --oneline -10
ls src/ 2>/dev/null || echo "src/ not created yet"

# 3. Do ONE small task (see "What is ONE task?" below)

# 4. Record what you did
echo "Iteration N: <what you did>" >> progress.txt

# 5. If ALL work is complete (ALL 7 phases done, ALL 12 success criteria met):
echo "<promise>COMPLETE</promise>"

# 6. EXIT — do nothing else
```

---

## What is ONE task?

**Batch similar/mechanical work together.** If the changes follow the same pattern (same transformation applied to multiple types/handlers), do them all in one iteration. Don't waste iterations doing one type at a time — they're the same pattern with different type names.

**YES — do these (each is one iteration):**
- Add ALL agent config types (`AgentConfig`, `AgentRoleConfig`, `ToolEntry`) to `Config` + parse from `loopr.yml`
- Create ALL agent foundation types (`AgentType`, `AgentStatus`, `AgentSession`, `AgentAction`) in `src/agents/mod.rs`
- Implement `Record` for `AgentSession` (TaskStore persistence)
- Create `AgentIpcBridge` for in-process daemon communication
- Add ALL `agent.*` handlers (`start`, `stop`, `pause`, `resume`, `status`, `list`)
- Create the full `ToolRunner` module (`src/tools/mod.rs` — `ToolRunner`, `ToolResult`, subprocess execution, timeout, truncation)
- Create the Implementer agent loop (`src/agents/implementer.rs` — context loading, prompt construction, action parsing, action execution)
- Create the `AgentLlmClient` with streaming SSE (`reqwest` async, Anthropic Messages API)
- Add `AgentEvent` variants to the event system + broadcast channel integration
- Create the Reviewer agent loop (`src/agents/reviewer.rs` — review context, prompt, ReviewResult parsing)
- Implement staleness cascade (listen for tick.published, detect stale agents, refresh worktrees)
- Add Agent view to TUI (list agents, show live output)
- Add `loopr agent *` CLI commands
- Fix a compile error or test failure

**NO — too much:**
- "Implement Phase 2" (Implementer + tools + tests = multiple logical units)
- Add agent system + tool system + streaming in one iteration
- Mix unrelated work (e.g., fix a bug AND add a new feature)

## On Previous Validation Failure

If the injected context below shows a FAIL or validation output with errors:
1. Read the validation/quality gate output (injected below — you already have it)
2. For more detail, read `logs/iter-NNN-validation.log` and `logs/iter-NNN-claude.log` for the failing iteration
3. Fix that ONE thing
4. Record what you fixed
5. EXIT immediately

---

## Project: Loopr v3 MVP3

### What You're Building

Loopr is a TUI-based "dev team in a box" orchestrator. MVP3 transforms Loopr from a human-driven orchestrator into the "dev team in a box" it was designed to be. Three new subsystems:

1. **Agent System** — LLM agents (Implementer + Reviewer) running as Tokio tasks inside the daemon. Each agent iteration is a Ralph Wiggum Loop: load context from TaskStore, construct prompt, call LLM, parse structured actions, execute actions via IPC + tools, persist results.
2. **Tool Execution System** — OS subprocesses in worktrees. Configured tool catalog (`cargo test`, `cargo clippy`, etc.) with timeouts, output truncation, and structured results.
3. **Agent Streaming** — Real-time delivery of agent output (LLM tokens, tool stdout/stderr, status changes) to the TUI via the existing broadcast channel.

### Starting Point

**MVP1 + MVP2 are complete.** The codebase has:
- ~17,400 lines of Rust across 41 `.rs` files
- 7 domain types with 3 FSMs (WorkItem, Bundle, Tick) + HierarchyStatus
- Daemon with IPC handlers over Unix socket (NDJSON)
- TaskStore persistence (JSONL-as-truth, SQLite-as-cache) via `scottidler/taskstore`
- Doc Validator (read-only LLM gating Draft→Active transitions) using `ureq`
- Ratatui TUI with 5 views + CLI dispatch
- Git worktree management with staleness guards
- Crash recovery on startup

### Adding Dependencies

**ALWAYS use `cargo add` to add dependencies.** Never hand-write version numbers in `Cargo.toml`.

```bash
# For reqwest (async HTTP client for agent LLM calls):
cargo add reqwest --features json,stream

# For tokio-stream (SSE parsing — may already be present):
cargo add tokio-stream
```

**Keep `ureq` for DocValidator** (sync handler context). Use `reqwest` (async) for agents (Tokio task context). Two different HTTP clients for two different execution contexts.

**Read the design doc:** `docs/design/2026-02-26-loopr-v3-mvp3.md`
This is the single source of truth for MVP3. It contains all data models, API changes, implementation phases, and success criteria.

**Previous design docs (reference):**
- `docs/design/2026-02-25-loopr-v3-mvp1.md` — orchestration spine
- `docs/design/2026-02-26-loopr-v3-mvp2.md` — TaskStore + Doc Validator

### Architecture Summary

- **Single Rust binary** (`loopr`) operating in daemon or TUI/CLI mode
- **Daemon** — Tokio process owning all mutable state, validates FSM transitions, manages worktrees, broadcasts events
- **Agent System** — Tokio tasks inside daemon. Implementer agents take WorkItems, work in worktrees, produce Bundles. Reviewer agents review Bundles.
- **AgentIpcBridge** — In-process channel for agent↔daemon communication. Same `dispatch()` function as socket-based IPC — same FSM validation, role guards, parent checks apply.
- **Tool System** — OS subprocesses (`Command::new("sh").arg("-c")`) in worktrees. Configured timeouts, output truncation.
- **AgentLlmClient** — `reqwest` async HTTP client with SSE streaming for Anthropic Messages API. Separate from MVP2's `ureq`-based DocValidator client.
- **TaskStore** — `Arc<Mutex<Store>>` — JSONL truth + SQLite cache. `AgentSession` is a new record type.

### Key Technical Details

**Agents run as Tokio tasks, not separate processes.** They're spawned by the daemon via `agent.start` handler. Agent crash → Failed status, daemon continues (Tokio task panic handling).

**Agent actions go through FSM validation.** An agent cannot force an invalid state transition. The daemon rejects it the same way it would reject an invalid TUI command.

**Two HTTP clients for two contexts:**
- `ureq` (sync) — DocValidator in sync handler context (MVP2, unchanged)
- `reqwest` (async) — AgentLlmClient in Tokio task context (MVP3)

**reqwest::blocking panics inside Tokio.** Never use it. Agents are Tokio tasks, so async `reqwest::Client` is correct.

**Path validation for WriteFile:** Agent file writes are sandboxed to the worktree. Paths are canonicalized and rejected if they escape the worktree root.

**Streaming:** LLM tokens, tool output, and status changes flow through the existing `broadcast::Sender<DaemonEvent>` channel to TUI.

### New Modules (target)

```
src/
├── agents/
│   ├── mod.rs           # AgentType, AgentStatus, AgentSession, AgentAction, AgentEvent
│   ├── implementer.rs   # Implementer agent loop
│   ├── reviewer.rs      # Reviewer agent loop
│   ├── bridge.rs        # AgentIpcBridge (in-process daemon communication)
│   └── llm_client.rs    # AgentLlmClient (reqwest, streaming SSE)
├── tools/
│   └── mod.rs           # ToolRunner, ToolResult, subprocess execution
└── (existing modules updated)
```

### Implementation Phases

#### Phase 1: Agent Foundation
- Add `AgentConfig`, `AgentRoleConfig`, `ToolEntry` to `Config`
- Create `src/agents/mod.rs` — `AgentType`, `AgentStatus`, `AgentSession` types
- Implement `Record` for `AgentSession` (TaskStore persistence)
- Create `AgentIpcBridge` for in-process daemon communication
- Add `agent.start`, `agent.stop`, `agent.pause`, `agent.resume`, `agent.status`, `agent.list` handlers
- Agent session lifecycle management (create, track, cleanup)

#### Phase 2: Implementer Agent
- Create `src/agents/implementer.rs` — the Implementer agent loop
- Implement context loading (hierarchy traversal, Learnings, worktree state)
- Implement prompt construction from system prompt + context
- Add `AgentAction` enum and JSON parsing
- Implement action execution (WriteFile, ReadFile, Commit, ProposeBundle, etc.)
- Wire Implementer into Tokio task spawning from `agent.start` handler
- Iteration tracking and `max_iterations` safety cap
- Error handling and graceful failure

#### Phase 3: Tool Execution System
- Create `src/tools/mod.rs` — `ToolRunner`, `ToolResult`
- Implement `ToolRunner::run()` with async subprocess execution (`tokio::process::Command`)
- Add timeout enforcement with SIGTERM → SIGKILL escalation
- Output truncation for context window management (32KB cap)
- Tool catalog loading from config
- Integration with agent action execution

#### Phase 4: Streaming
- Add `AgentEvent` variants to the event system
- Implement `AgentLlmClient` with `reqwest` (async) for Anthropic Messages API
- Implement SSE streaming (message_start, content_block_delta, message_stop)
- Forward LLM token chunks through broadcast channel
- Forward tool output through broadcast channel
- Agent status change events

#### Phase 5: Reviewer Agent
- Create `src/agents/reviewer.rs` — the Reviewer agent loop
- Implement review context loading (Bundle diff, hierarchy, Learnings)
- Implement review prompt construction
- Parse `ReviewResult` from LLM response (verdict, issues, summary)
- Execute review actions (transition Bundle, create Learning)
- Wire Reviewer into Tokio task spawning

#### Phase 6: Staleness Cascade
- Listen for `tick.published` events in agent system
- Identify stale Implementer agents (compare base_tick_id)
- Set stale flag and inject staleness context into next iteration
- Worktree refresh on stale detection
- Update WorkItem `base_tick_id` after refresh

#### Phase 7: TUI + CLI + Polish
- Add Agent view to TUI (list agents, show live output)
- Add `loopr agent *` CLI commands (start, stop, pause, resume, list, status)
- Agent session detail view (iterations, actions, tool results)
- Dashboard integration (agent count, active/paused status)
- Comprehensive integration tests
- Documentation

### Phase Dependencies

```
Phase 1 (Agent Foundation)
    │
    ├──→ Phase 3 (Tool System) ──┐
    │                            ├──→ Phase 2 (Implementer) ──→ Phase 4 (Streaming)
    └────────────────────────────┘         │                          │
                                           │                          │
                                           └──→ Phase 5 (Reviewer) ──┘
                                                                      │
                                           Phase 6 (Staleness) ──────┘
                                               │
                                               └──→ Phase 7 (TUI + CLI + Polish)
```

Phase 2 (Implementer) depends on both Phase 1 (Foundation) and Phase 3 (Tool System). Phase 3 can be developed in parallel with Phase 1. Phase 5 (Reviewer) depends on Phase 2. Phase 6 (Staleness) is independent until integration.

### Success Criteria (ALL must be met)

1. Implementer agent takes a WorkItem from InProgress and produces a Bundle
2. Implementer agent runs tools (test, clippy) before proposing Bundle
3. Reviewer agent reviews a Bundle and provides structured feedback
4. Agent output streams to TUI in real-time
5. Staleness cascade notifies agents when Tick publishes
6. Agent pool bounds are enforced (max concurrent agents)
7. Agent actions go through FSM validation (invalid transitions rejected)
8. Agents disabled by default (`agents.enabled = false`), human-driven workflow unchanged
9. Agent sessions persist in TaskStore (survive daemon restart)
10. Max iterations cap prevents runaway agents
11. Coordinator can pause/resume/stop agents
12. Tool timeout kills runaway subprocesses

---

## Rust Conventions

1. **Async everywhere** — tokio runtime, async fn
2. **Structured errors** — thiserror for types, eyre/anyhow for propagation
3. **Return data** — functions return `Result<T>`, minimize side effects
4. **Tests in every module** — `#[cfg(test)] mod tests { ... }` at the bottom of each file
5. **Use `cargo add`** for dependencies — never manually write version numbers in Cargo.toml. Your training data versions are stale; `cargo add` fetches the latest from crates.io.

### Testing Requirements

Every module must have tests. When you write code, write tests for it.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_something() {
        // ...
    }
}
```

- Test public functions and key internal logic
- Use `#[tokio::test]` for async tests
- For the Agent LLM client: use a mock/trait-based interface, never hit real API in tests
- For tool execution: mock subprocess where possible, test timeout/truncation logic
- Meaningful coverage, not 100% coverage theater

### Key Dependencies

- `tokio` — async runtime (full features)
- `serde`, `serde_json` — serialization
- `ratatui`, `crossterm` — TUI
- `tokio-util` — NDJSON codec (LinesCodec)
- `thiserror` — error types
- `eyre` — error propagation
- `clap` — CLI argument parsing
- `env_logger` — logging
- `taskstore` — external crate from `scottidler/taskstore` (git dependency) — JSONL + SQLite persistence
- `ureq` — sync HTTP client for Doc Validator (MVP2, unchanged)
- `reqwest` — async HTTP client for Agent LLM calls (MVP3, streaming SSE)

---

## Completion

Output `<promise>COMPLETE</promise>` on its own line when you believe:
- ALL 7 phases are implemented
- ALL 12 success criteria are met
- Code compiles and tests pass
- No `#[allow(dead_code)]` remaining
- No underscore-prefixed unused variables

The bash loop verifies this externally. If validation fails, you'll be restarted.

---

## If You Get Stuck

1. Record the blocker in progress.txt
2. EXIT immediately
3. The next iteration sees the blocker and can try a different approach

**NEVER just exit.** Always update progress.txt first.

---

## Logs

Per-iteration logs are saved in `logs/`:
- `logs/iter-NNN-claude.log` — Full Claude output for iteration NNN
- `logs/iter-NNN-validation.log` — Full `otto ci` validation output for iteration NNN

If you need to investigate what happened in a previous iteration (e.g., why a change didn't work, what code was generated), read the relevant log files.

## Quick Reference

| File | Purpose |
|------|---------|
| `progress.txt` | Your memory between iterations |
| `logs/iter-NNN-claude.log` | Full Claude output for iteration NNN |
| `logs/iter-NNN-validation.log` | Full validation output for iteration NNN |
| `docs/design/2026-02-26-loopr-v3-mvp3.md` | **The MVP3 design doc** — single source of truth |
| `docs/design/2026-02-26-loopr-v3-mvp2.md` | MVP2 design doc (reference) |
| `docs/design/2026-02-25-loopr-v3-mvp1.md` | MVP1 design doc (reference) |
| `docs/mvps.md` | MVP1/2/3 comparison matrix |
| `.otto.yml` | CI pipeline definition |

## Start of Iteration Checklist

1. [ ] Read injected context below (progress + validation + quality gates)
2. [ ] If last iteration failed: fix that failure. If not: determine next task.
3. [ ] Optionally run `git log --oneline -10` and `ls src/` for current state
4. [ ] Do one logical unit of work — batch similar/mechanical changes together
5. [ ] `git add <specific files>` then `git commit`
6. [ ] `echo "Iteration N: <what you did>" >> progress.txt`
7. [ ] Check if ALL phases + success criteria complete → `echo "<promise>COMPLETE</promise>"`
8. [ ] EXIT
