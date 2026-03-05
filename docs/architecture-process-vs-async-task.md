# Loopr Architecture: Process vs Lightweight Async Task

## The Core Distinction: Everything is a Tokio Task

**There are no separate OS processes for agents.** Every agent in loopr — whether "lightweight" or "heavy" — runs as a **tokio async task** inside the single daemon process. The real architectural distinction is between two planes:

## 1. Thinking Plane Agents (no worktree, no subprocess)

These are identified by `AgentType::is_thinking_plane()` returning `true`:
- **Coordinator** — orchestrates the overall plan, spawns other agents
- **Researcher** — investigates the codebase (read-only)
- **Reviewer** — reviews bundles of changes
- **Integrator** — merges approved bundles

**Characteristics:**
- No git worktree created (`worktree_key = None` in `executor.rs:139`)
- Operate on the main repo or in-memory state
- Their filesystem ops use the repo root directly (`stores.config.project.repo_path`)

**How Researcher does file ops** (`researcher.rs`):
- `SearchCode` → spawns `tokio::process::Command::new("rg")` or `grep` as a **child subprocess** with 30s timeout
- `SearchFiles` → uses the `glob` crate (pure Rust, in-process)
- `ListDirectory` → uses `tokio::fs::read_dir` (async, in-process)
- `ReadFile` → uses `tokio::fs::read_to_string` (async, in-process)

So even "lightweight" agents spawn subprocesses for specific operations — but the agent loop itself is just an async task.

## 2. Action Plane Agents (worktree + subprocesses)

Only **Implementer** currently:
- Gets a dedicated **git worktree** (`worktree_mgr.get_or_create(key, base_ref)` in `executor.rs:164`)
- The worktree is keyed by `work_id`, based off the latest Published Tick's SHA
- Runs tools (test, lint, build) as **subprocesses** via `ToolRunner::run()` (`tools/mod.rs:131`)

**ToolRunner mechanics** (`tools/mod.rs`):
- Spawns `sh -c "<command>"` via `tokio::process::Command`
- Sets `current_dir` to the worktree path
- Enforces timeout with SIGTERM→SIGKILL escalation
- Truncates output to 32KB per stream
- `kill_on_drop(true)` for safety

## The Spawn Chain

```
daemon_main()
  └─ handle_agent_start()                    [sync handler]
       └─ creates AgentSession (Starting)
       └─ stores JoinHandle in DaemonContext
       └─ tokio::spawn(run_agent_task(...))   [async task]
            └─ creates worktree (if action plane)
            └─ creates AgentLogger (per-session file)
            └─ creates AgentIpcBridge (in-process)
            └─ run_agent_loop()
                 └─ creates LLM client (reqwest HTTP)
                 └─ constructs agent struct
                 └─ agent.run() — the iteration loop
                      └─ LLM call (SSE streaming via reqwest)
                      └─ parse actions from response
                      └─ execute_action() per action
                           └─ RunTool → tokio::process::Command
                           └─ WriteFile → tokio::fs::write
                           └─ SearchCode → tokio::process::Command("rg")
                           └─ Commit → std::process::Command("git")
                           └─ bridge.request() → in-memory IPC
```

## Where Subprocesses Actually Live

| Operation | Mechanism | Where |
|-----------|-----------|-------|
| LLM calls | `reqwest` HTTP client, SSE streaming | In-process async |
| File read/write | `tokio::fs` | In-process async |
| Glob search | `glob` crate | In-process sync |
| Code search (rg/grep) | `tokio::process::Command` | **Child process** |
| Git operations | `std::process::Command` | **Child process** (blocking!) |
| Tool execution (test/lint/build) | `tokio::process::Command` | **Child process** |
| Worktree management | `std::process::Command("git")` | **Child process** |

## The Worker Pool Model

There's also a **pull-based worker pool** (`worker.rs`, `daemon/mod.rs:190`):
- N persistent tokio tasks that poll for `Ready` Work items
- Each worker calls `run_single_work()` which runs the full implementer loop
- Alternative to the push-based `auto_start_implementer` model
- Controlled by `config.agents.pull_based_workers`

## The Supervisor

`supervisor.rs` watches for Coordinator failures via the broadcast event channel and restarts with exponential backoff (up to `max_restarts`). It's another tokio task, not a separate process.

## Key Architectural Point: The IPC Bridge

Agents don't talk to the daemon via the Unix socket — they use an **in-process bridge** (`AgentIpcBridge`). This means:
- Agent → `bridge.request("work.transition", ...)` → directly calls the handler functions
- No serialization overhead, no socket round-trip
- The bridge holds `Arc<Stores>` and `broadcast::Sender<DaemonEvent>`

## Summary

The architecture is **single process, multi-task**. The daemon is one OS process with:
- A Unix socket listener for external IPC (TUI, CLI)
- N tokio tasks for agents (each with its own iteration loop + LLM client)
- Child subprocesses spawned on-demand for shell commands (git, rg, test runners)

The "lightweight vs heavy" distinction is really about:
1. **Does it need a worktree?** (thinking plane = no, action plane = yes)
2. **Does it spawn subprocesses?** (all agents can, but Implementer does it most)
3. **Does it have an LLM?** (all except Integrator, which is deterministic)
