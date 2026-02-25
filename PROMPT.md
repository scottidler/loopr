# Ralph Wiggum Loop — ONE TASK THEN EXIT

You are in a Ralph Wiggum loop. You have NO MEMORY of previous runs.
Your state persists ONLY in `progress.txt` and the git history.

## CRITICAL RULES

1. **READ progress.txt FIRST** — it tells you what was done
2. **DO ONE SMALL THING** — not a phase. One file, one fix, one test.
3. **EXIT IMMEDIATELY** — do not retry failures. Do not loop. Just exit.

The bash loop restarts you with fresh context. That's the whole point.
The bash loop runs validation EXTERNALLY — you do NOT run `otto ci`.

---

## Your Workflow

```bash
# 1. Read state (MANDATORY — do this FIRST)
cat progress.txt
git log --oneline -10
ls src/ 2>/dev/null || echo "src/ not created yet"

# 2. Do ONE small task (see "What is ONE task?" below)

# 3. Record what you did
echo "Iteration N: <what you did>" >> progress.txt

# 4. If ALL work is complete (ALL 6 phases done, ALL 11 success criteria met):
echo "<promise>COMPLETE</promise>"

# 5. EXIT — do nothing else
```

---

## What is ONE task?

**YES — do these:**
- Add `src/domain/plan.rs` with struct + Record impl + tests
- Fix the compile error in `src/daemon/handlers.rs`
- Add FSM transition tests for BundleStatus
- Wire up the `work_item.transition` IPC handler
- Add the Dashboard TUI view

**NO — too much:**
- "Implement Phase 2"
- Add multiple modules in one go
- Fix errors then add features then refactor

## On Previous Validation Failure

If progress.txt shows a FAIL from the previous iteration:
1. Read what failed
2. Fix that ONE thing
3. Record what you fixed
4. EXIT immediately

---

## Project: Loopr v3 MVP1

### What You're Building

Loopr is a TUI-based "dev team in a box" orchestrator. MVP1 proves the orchestration spine with zero LLM involvement — the human acts as every persona via the TUI.

**Read the design doc:** `docs/design/2026-02-25-loopr-v3-mvp1.md`
This is the single source of truth. It contains all data models, FSMs, IPC protocol, TUI design, and architecture decisions.

**Supporting docs (read as needed):**
- `docs/AGENT.md` — reading order and doc map
- `docs/mvps.md` — MVP1/2/3+ comparison matrix
- `docs/v2-proven-patterns.md` — infrastructure patterns to reuse

### Architecture Summary

- **Single Rust binary** (`loopr`) operating in daemon or TUI/CLI mode
- **Daemon** — Tokio process owning all mutable state (TaskStore), validates FSM transitions, manages worktrees, broadcasts events
- **TUI** — thin ratatui client connected via Unix socket NDJSON IPC, never touches storage
- **TaskStore** — external crate (`scottidler/taskstore`), JSONL truth + SQLite cache
- **3 FSMs** — WorkItem (8 states), Bundle (8 states), Tick (5 states) + HierarchyStatus for Plan/Spec/Phase
- **Roles** — Coordinator, Integrator, Implementer (human switches via TUI)

### Crate Layout (target)

```
loopr/
├── Cargo.toml
├── build.rs
├── src/
│   ├── main.rs              # Entry: fork-to-daemon or connect-as-client
│   ├── lib.rs               # Module declarations
│   ├── domain/              # Core domain types and FSMs
│   │   ├── mod.rs
│   │   ├── plan.rs          # Plan record
│   │   ├── spec.rs          # Spec record
│   │   ├── phase.rs         # Phase record
│   │   ├── work_item.rs     # WorkItem record + FSM
│   │   ├── bundle.rs        # Bundle record + FSM
│   │   ├── tick.rs          # Tick record + FSM
│   │   ├── learning.rs      # Learning record
│   │   ├── lock.rs          # Lock record
│   │   ├── role.rs          # Role enum
│   │   └── transition.rs    # Shared transition validation
│   ├── daemon/              # Daemon process
│   │   ├── mod.rs           # Startup, main select! loop
│   │   ├── context.rs       # DaemonContext (shared state hub)
│   │   └── handlers.rs      # IPC request handlers
│   ├── ipc/                 # Inter-process communication
│   │   ├── mod.rs
│   │   ├── protocol.rs      # Request/Response/Event types
│   │   ├── server.rs        # Unix socket server (daemon side)
│   │   ├── client.rs        # Unix socket client (TUI/CLI side)
│   │   └── codec.rs         # NDJSON codec (tokio_util)
│   ├── tui/                 # Terminal UI
│   │   ├── mod.rs
│   │   ├── app.rs           # App state, event loop
│   │   ├── views/
│   │   │   ├── mod.rs
│   │   │   ├── dashboard.rs
│   │   │   ├── work_items.rs
│   │   │   ├── bundles.rs
│   │   │   ├── ticks.rs
│   │   │   └── learnings.rs
│   │   └── input.rs         # Keyboard handling
│   ├── worktree/            # Git worktree management
│   │   ├── mod.rs
│   │   └── manager.rs
│   ├── cli/                 # CLI commands (headless)
│   │   └── mod.rs
│   ├── config.rs            # Configuration
│   ├── error.rs             # Error types
│   └── id.rs                # ID generation
└── tests/
    └── integration/
```

### Implementation Phases

#### Phase 1: Foundation
- Domain types: all 8 record structs with serde derives and Record trait implementations
- FSM engine: TransitionRule, validate_transition(), role enum
- Error types (thiserror)
- ID generation (ULID or timestamp-based)
- Config parsing (toml)
- **Tests:** unit tests for every FSM transition (valid AND invalid), record serialization round-trips, invariant checks

#### Phase 2: Daemon Core
- DaemonContext struct
- Unix socket server with NDJSON codec (tokio_util FramedRead/Write + serde_json)
- Client connection handling (tokio::spawn per client)
- Version handshake (system.handshake)
- PID file lifecycle (write on start, remove on shutdown)
- Broadcast channel for events (tokio::sync::broadcast)
- **Tests:** integration test — start daemon, connect client, send handshake, receive response

#### Phase 3: Handlers + Worktree Manager
- CRUD handlers for all 8 record types
- FSM transition handlers with role checking
- Event broadcasting on state changes
- WorktreeManager: create/list/cleanup/refresh worktrees
- Worktree-related IPC handlers
- **Tests:** integration tests — create records, transition states, verify events, create/cleanup a worktree

#### Phase 4: TUI
- App struct and event loop (crossterm + ratatui)
- IPC client connection
- 5 views: Dashboard, WorkItems, Bundles, Ticks, Learnings
- Keyboard navigation (Tab, j/k, Enter, Esc, n, t, r, q, ?)
- Role switching (r key)
- Action bar filtered by current role
- **Tests:** manual testing — full end-to-end workflow through TUI

#### Phase 5: Integration Pipeline
- Bundle proposal workflow: collect touched paths + commit range from worktree, create Bundle record
- Staleness guard: reject bundle if base_tick_id is behind latest Published Tick
- Integrator validation command runner (sequential shell commands, capture output)
- Tick sealing, validation, publishing (record integration SHA on success)
- worktree.refresh IPC method
- **Tests:** integration test — full pipeline from plan creation to tick publication

#### Phase 6: CLI + Polish
- CLI commands via IPC (headless operation — all TUI actions available from command line)
- Crash recovery (detect orphaned InProgress WorkItems / Integrating Bundles on daemon startup)
- Graceful shutdown (SIGTERM handler, socket + PID cleanup)
- Logging (env_logger or tracing)
- Error messages and edge case handling
- **Tests:** CLI script exercising the full pipeline without TUI

### Success Criteria (ALL must be met)

MVP1 is complete when a human can do the following end-to-end:

1. Create a Plan → Spec → Phase → WorkItem hierarchy
2. Create a Git worktree for a WorkItem
3. (In a separate terminal) make changes in the worktree, commit them
4. Propose a Bundle from the worktree
5. Walk the Bundle through Proposed → Triaged → Reviewed → Accepted
6. Create a Tick, seal it, validate it (`cargo test`), and publish it
7. See the published Tick's Git SHA
8. All invalid transitions are rejected with clear error messages
9. Role switching changes available actions
10. Daemon survives TUI disconnect and reconnects cleanly
11. Bundle proposal is rejected when `base_tick_id` is behind latest Published Tick (staleness guard)

---

## Rust Conventions

1. **Async everywhere** — tokio runtime, async fn
2. **Structured errors** — thiserror for types, eyre/anyhow for propagation
3. **Return data** — functions return `Result<T>`, minimize side effects
4. **Tests in every module** — `#[cfg(test)] mod tests { ... }` at the bottom of each file
5. **Use `cargo add`** for dependencies — never manually write version numbers in Cargo.toml

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
- Meaningful coverage, not 100% coverage theater

### Key Dependencies

- `tokio` — async runtime (full features)
- `serde`, `serde_json` — serialization
- `ratatui`, `crossterm` — TUI
- `tokio-util` — NDJSON codec (LinesCodec)
- `thiserror` — error types
- `toml` — config parsing
- `clap` — CLI argument parsing
- `tracing` or `env_logger` — logging
- `taskstore` — external crate from `scottidler/taskstore` (git dependency)

---

## Completion

Output `<promise>COMPLETE</promise>` on its own line when you believe:
- ALL 6 phases are implemented
- ALL 11 success criteria are met
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

## Quick Reference

| File | Purpose |
|------|---------|
| `progress.txt` | Your memory between iterations |
| `docs/design/2026-02-25-loopr-v3-mvp1.md` | **The design doc** — single source of truth |
| `docs/AGENT.md` | Doc map and reading order |
| `docs/v2-proven-patterns.md` | Infrastructure patterns from v2 |
| `.otto.yml` | CI pipeline definition |

## Start of Iteration Checklist

1. [ ] Read `progress.txt`
2. [ ] Run `git log --oneline | head -10`
3. [ ] Run `ls src/` to see current state
4. [ ] Determine what to work on next
5. [ ] Do ONE small task (implement code + write tests)
6. [ ] `git add <specific files>` then `git commit`
7. [ ] `echo "Iteration N: <what you did>" >> progress.txt`
8. [ ] Check if ALL phases + success criteria complete → `echo "<promise>COMPLETE</promise>"`
9. [ ] EXIT
