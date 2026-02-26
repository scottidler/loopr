# Ralph Wiggum Loop — ONE TASK THEN EXIT

You are in a Ralph Wiggum loop. You have NO MEMORY of previous runs.
Your state persists ONLY in `progress.txt` and the git history.

## CRITICAL RULES

1. **CHECK THE INJECTED CONTEXT BELOW** — progress, validation output, and quality gate results from previous iterations are appended to this prompt automatically. You do NOT need to read progress.txt yourself.
2. **DO ONE SMALL THING** — not a phase. One file, one fix, one test.
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

# 5. If ALL work is complete (ALL 6 phases done, ALL 10 success criteria met):
echo "<promise>COMPLETE</promise>"

# 6. EXIT — do nothing else
```

---

## What is ONE task?

**YES — do these:**
- Add `Record` trait impl for Plan in `src/domain/plan.rs`
- Add `taskstore` git dependency to `Cargo.toml`
- Replace HashMap store with TaskStore in `DaemonContext`
- Migrate `plan.create` handler to use TaskStore
- Add `ValidatorConfig` to `Config`
- Add `validator.validate` handler
- Fix the compile error in `src/daemon/handlers.rs`
- Wire TUI event processing to refresh AppState

**NO — too much:**
- "Implement Phase 2"
- Migrate all 8 handler groups in one go
- Add validator module + wire handlers + add gate in one iteration

## On Previous Validation Failure

If the injected context below shows a FAIL or validation output with errors:
1. Read the validation/quality gate output (injected below — you already have it)
2. For more detail, read `logs/iter-NNN-validation.log` and `logs/iter-NNN-claude.log` for the failing iteration
3. Fix that ONE thing
4. Record what you fixed
5. EXIT immediately

---

## Project: Loopr v3 MVP2

### What You're Building

Loopr is a TUI-based "dev team in a box" orchestrator. MVP2 adds two capabilities to the MVP1 spine:

1. **TaskStore Persistence** — Replace in-memory HashMaps with `taskstore::Store` (JSONL-as-truth, SQLite-as-cache) so state survives daemon restarts.
2. **Doc Validator** — A read-only LLM that gates Plan/Spec/Phase quality before `Draft → Active` transitions. The safest possible entry point for intelligence — can't break Tick semantics.

### Starting Point

**MVP1 is complete.** The codebase has:
- 12,676 lines of Rust across 37 `.rs` files
- 445+ tests (100% module coverage)
- 7 domain types with 3 FSMs (WorkItem, Bundle, Tick) + HierarchyStatus
- Daemon with 40 IPC handlers over Unix socket (NDJSON)
- Ratatui TUI with 5 views + CLI dispatch
- Git worktree management with staleness guards
- Crash recovery on startup
- All state is in-memory HashMap — **this is what you're replacing**

### Adding Dependencies

**ALWAYS use `cargo add` to add dependencies.** Never hand-write version numbers in `Cargo.toml`.

```bash
# For TaskStore (git dependency):
cargo add taskstore --git https://github.com/scottidler/taskstore.git

# For ureq (sync HTTP client for LLM API):
cargo add ureq --features json
```

**Read the design doc:** `docs/design/2026-02-26-loopr-v3-mvp2.md`
This is the single source of truth for MVP2. It contains all data models, API changes, implementation phases, and success criteria.

**MVP1 design doc (reference):** `docs/design/2026-02-25-loopr-v3-mvp1.md`

### Architecture Summary

- **Single Rust binary** (`loopr`) operating in daemon or TUI/CLI mode
- **Daemon** — Tokio process owning all mutable state, validates FSM transitions, manages worktrees, broadcasts events
- **TaskStore** — external crate (`scottidler/taskstore`), JSONL truth + SQLite cache. Replaces `Stores` (8 HashMaps). Store is `Arc<Mutex<taskstore::Store>>`.
- **Doc Validator** — `ureq`-based sync HTTP client calling Anthropic API. Returns `ValidationReport`. Opt-in via `validator.enabled` config.
- **TUI** — thin ratatui client connected via Unix socket NDJSON IPC. Event-driven AppState refresh (fix existing TODO).

### Key Technical Details

**TaskStore is synchronous.** `dispatch()` in `handlers.rs` is a sync function — this is compatible. Use `std::sync::Mutex` (not `tokio::sync::Mutex`).

**Error mapping.** TaskStore returns `eyre::Result<T>`. Handlers return `DaemonResponse`. Map with:
```rust
store.lock().unwrap().get::<Plan>(&id)
    .map_err(|e| RpcError::internal(&e.to_string()))?;
```

**Timestamp management.** Before `store.update()`, update `updated_at`:
```rust
record.updated_at = id::now_millis();
store.lock().unwrap().update(record)?;
```

**Index rebuilding.** On daemon startup after `Store::open()`, call `rebuild_indexes::<T>()` for all record types.

**reqwest::blocking is NOT an option.** It panics inside Tokio runtime ("Cannot start a runtime from within a runtime"). Use `ureq` instead — it's purely synchronous with no async runtime.

**Record trait.** Each domain type implements `taskstore::Record`:
```rust
impl Record for Plan {
    fn id(&self) -> &str { &self.id }
    fn updated_at(&self) -> i64 { self.updated_at }
    fn collection_name() -> &'static str { "plans" }
    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m
    }
}
```

### New Modules (target)

```
src/
├── domain/
│   └── validation.rs    # ValidationReport, ValidationVerdict, ValidationIssue
├── validator/
│   ├── mod.rs           # DocValidator struct
│   ├── client.rs        # LLM API client (ureq)
│   └── prompts.rs       # Per-type prompt templates
└── (existing modules updated)
```

### Implementation Phases

#### Phase 1: TaskStore Integration (Foundation)
- Add `taskstore` git dependency
- Implement `Record` trait for all 8 domain types
- Replace `Stores` struct with `Arc<Mutex<Store>>` in `DaemonContext`
- Update `DaemonContext::new()` to call `Store::open()` on startup
- Call `rebuild_indexes::<T>()` for all types after open
- Update crash recovery to use TaskStore get/update
- Add `system.init` handler
- **Tests:** Record trait roundtrips, indexed fields, DaemonContext with TaskStore

#### Phase 2: Handler Migration
- Migrate all `*.create` handlers (8 handlers): HashMap insert → `store.create()`
- Migrate all `*.get` handlers (8 handlers): HashMap get → `store.get::<T>()`
- Migrate all `*.list` handlers (8 handlers): HashMap values → `store.list::<T>()`
- Migrate all `*.transition` handlers (4 handlers): get + mutate + update
- Migrate learning/lock action handlers (6 handlers)
- Migrate worktree + integrator handlers
- Update `system.status` to report TaskStore stats
- **Tests:** Create records, restart daemon, verify records survive

#### Phase 3: Doc Validator
- Add `validator` module with `DocValidator` struct
- Add `ValidatorConfig` to `Config`
- Implement LLM API client (HTTP via `ureq`) for Anthropic Messages API
- Add `ValidationReport` domain type with `Record` impl
- Define per-type validation prompt templates
- Add `validator.validate`, `validator.report`, `validator.reports` handlers
- **Tests:** Prompt construction, report parsing, mock LLM client

#### Phase 4: Validation Gate
- Add validation gate to `plan.transition`, `spec.transition`, `phase.transition` (Draft → Active)
- Add `-32003` error code for `validation_required`
- Add `--skip-validation` escape hatch for Coordinator
- When `validator.enabled = false` (default), gate is not applied
- **Tests:** Pass/fail/warn/missing report scenarios, skip-validation, disabled validator

#### Phase 5: TUI Event Processing
- Implement `refresh_collection()` — sends `*.list` IPC request, updates AppState
- Wire event handler in `tui/run.rs` to call `refresh_collection` on daemon events
- Add validation report display to TUI
- **Tests:** Event → state refresh cycle

#### Phase 6: CLI & Polish
- Add `loopr init` CLI command (calls `system.init`)
- Add `loopr validate <collection> <id>` CLI command
- Add `loopr report <id>` CLI command
- Update `loopr status` to show TaskStore stats
- Comprehensive integration tests

### Phase Dependencies

```
Phase 1 (Foundation) ──→ Phase 2 (Handler Migration) ──→ Phase 6 (CLI & Polish)
    │                                                          ↑
    ├──→ Phase 3 (Doc Validator) ──→ Phase 4 (Validation Gate)─┘
    │
    └──→ Phase 5 (TUI Events) ─────────────────────────────────┘
```

### Success Criteria (ALL must be met)

1. Create records, restart daemon, records still exist
2. `.taskstore/` directory contains JSONL files committed to git
3. `system.init` creates TaskStore and installs git merge driver
4. Crash recovery works with persistent state
5. `validator.validate` returns structured ValidationReport (when validator enabled)
6. Draft → Active blocked without passing validation (when validator enabled)
7. Draft → Active succeeds after passing validation
8. `--skip-validation` override works
9. TUI updates reactively on daemon events
10. Validator disabled by default, doesn't break existing flow (transitions work exactly as MVP1)

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
- For the Doc Validator: use a mock HTTP client trait, never hit real API in tests
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
- `ureq` — sync HTTP client for LLM API calls (NOT reqwest — panics inside Tokio)

---

## Completion

Output `<promise>COMPLETE</promise>` on its own line when you believe:
- ALL 6 phases are implemented
- ALL 10 success criteria are met
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
| `docs/design/2026-02-26-loopr-v3-mvp2.md` | **The MVP2 design doc** — single source of truth |
| `docs/design/2026-02-25-loopr-v3-mvp1.md` | MVP1 design doc (reference) |
| `docs/AGENT.md` | Doc map and reading order |
| `docs/mvps.md` | MVP1/2/3+ comparison matrix |
| `.otto.yml` | CI pipeline definition |

## Start of Iteration Checklist

1. [ ] Read injected context below (progress + validation + quality gates)
2. [ ] If last iteration failed: fix that failure. If not: determine next task.
3. [ ] Optionally run `git log --oneline -10` and `ls src/` for current state
4. [ ] Do ONE small task (implement code + write tests)
5. [ ] `git add <specific files>` then `git commit`
6. [ ] `echo "Iteration N: <what you did>" >> progress.txt`
7. [ ] Check if ALL phases + success criteria complete → `echo "<promise>COMPLETE</promise>"`
8. [ ] EXIT
