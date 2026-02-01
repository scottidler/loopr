# Design Document: Unwired Code Cleanup

**Author:** Claude (with Scott Idler)
**Date:** 2026-02-01
**Status:** Ready for Review
**Review Passes Completed:** 5/5

## Summary

The loopr codebase has significant disconnects between implemented modules and their integration points. This document describes how to remove dead code markers (`#[allow(dead_code)]`, underscore-prefixed parameters) by either wiring up the code properly or deleting truly unused code. The core issue is that `LoopRunner` was built despite docs explicitly saying not to—the design calls for `impl Loop { fn run() }` instead.

## Problem Statement

### Background

Loopr v2 was built iteratively across 96 iterations. The implementation followed `implementation-phases.md` which specified creating a `LoopRunner` struct. However, `domain-types.md` explicitly states:

> "The previous design had three types: `LoopConfig` → `Loop` → `LoopRunner`. But `LoopRunner` was unnecessary... `LoopRunner` added nothing. It was indirection without value."

The docs are internally inconsistent, and the implementation followed the wrong spec. **This documentation inconsistency is the root cause of the entire problem.** Had the docs been consistent, `LoopRunner` would never have been built.

### Problem

1. **8 `#[allow(dead_code)]` markers** hide unused code that should either be wired up or deleted
2. **7+ underscore-prefixed parameters** indicate functions that don't use their arguments
3. **main.rs is a stub** that ignores CLI commands and never starts TUI or daemon
4. **LoopManager has `llm_client` and `tool_router` fields that are never used**
5. **LoopRunner exists but should be deleted** per domain-types.md

### Goals

- Remove ALL `#[allow(dead_code)]` markers
- Remove ALL underscore-prefixed parameters (except in trait implementations where required)
- Wire up main.rs to handle CLI subcommands
- Either use or delete every field/function currently marked as dead
- Align implementation with domain-types.md (Loop is self-contained)
- **Establish documentation consistency as a project mandate:**
  - All docs must be internally consistent (no contradictions between docs)
  - All docs must match what the code actually does
  - Define single source of truth for each concept
  - Audit and fix all existing documentation inconsistencies

### Non-Goals

- Full TUI implementation (just wire up the entry point)
- Full daemon implementation (just wire up the entry point)
- Adding new features
- Changing the public API beyond what's needed for cleanup

## Proposed Solution

### Overview

The fix has two parts:

1. **Delete LoopRunner** and move `run()` logic into `impl Loop`
2. **Wire up or delete** every piece of dead code

### Architecture Change

**Current (Wrong):**
```
Loop (data only) → LoopRunner (execution) → LoopManager (orchestration)
```

**Target (Per domain-types.md):**
```
Loop (data + execution via run()) → LoopManager (orchestration)
```

### Detailed Changes

#### 1. Delete `src/runner/loop_runner.rs`

The entire file should be deleted. The `LoopRunner` struct, `LoopRunnerConfig`, `SignalChecker` trait, and `NoOpSignalChecker` all go away.

**Migrate to Loop:**
- Move `run()` method to `impl Loop` in `src/domain/loop_record.rs`
- Move `build_system_prompt()`, `build_user_message()`, `get_artifact_path()` as private methods on Loop
- `get_tools_for_loop_type()` becomes a method that takes `&self` and uses `self.loop_type`
- `run()` takes `Arc<dyn LlmClient>`, `Arc<dyn ToolRouter>`, and `Arc<dyn Validator>` parameters
- `PromptRenderer` can be created inside `run()` or passed in—keep it simple

#### 2. Update `src/runner/mod.rs`

Keep only `LoopOutcome` enum (it's a valid return type). Delete re-exports of deleted items.

```rust
// src/runner/mod.rs - AFTER
mod outcome;
pub use outcome::LoopOutcome;
```

Or just move `LoopOutcome` to `src/domain/` and delete the runner module entirely.

#### 3. Fix `src/manager/loop_manager.rs`

**Remove dead fields:**
```rust
// BEFORE
pub struct LoopManager<S: Storage, L: LlmClient, T: ToolRouter> {
    storage: Arc<S>,
    #[allow(dead_code)]
    llm_client: Arc<L>,
    #[allow(dead_code)]
    tool_router: Arc<T>,
    ...
}

// AFTER - USE THE FIELDS
pub struct LoopManager<S: Storage, L: LlmClient, T: ToolRouter> {
    storage: Arc<S>,
    llm_client: Arc<L>,
    tool_router: Arc<T>,
    ...
}
```

**Fix `start_loop()` to actually execute:**
```rust
pub async fn start_loop(&self, loop_id: &str) -> Result<()> {
    // Fetch loop from storage
    let loop_opt: Option<Loop> = self.storage.get(LOOPS_COLLECTION, loop_id)?;
    let mut loop_instance = loop_opt.ok_or_else(|| LooprError::LoopNotFound(loop_id.into()))?;

    // Create worktree
    let worktree_path = self.worktree_manager.create(loop_id)?;
    loop_instance.worktree = worktree_path.clone();
    loop_instance.status = LoopStatus::Running;
    self.storage.update(LOOPS_COLLECTION, loop_id, &loop_instance)?;

    // Clone dependencies for the spawned task
    let llm = self.llm_client.clone();
    let tools = self.tool_router.clone();
    let storage = self.storage.clone();
    let validator = self.validator.clone();  // LoopManager needs a validator field too

    let handle = tokio::spawn(async move {
        loop_instance.run(llm, tools, validator).await
    });

    self.running_loops.write().await.insert(loop_id.to_string(), handle);
    Ok(())
}
```

**Note:** LoopManager will need a `validator: Arc<dyn Validator>` field added.

#### 4. Fix `src/main.rs`

**BEFORE:**
```rust
fn run_application(_cli: &Cli, config: &Config) -> Result<()> {
    println!("Hello from loopr!");
    ...
}
```

**AFTER:**
```rust
fn run_application(cli: &Cli, config: &Config) -> Result<()> {
    match &cli.command {
        None => {
            // Default: launch TUI (see docs/tui.md and tui-image1.png, tui-image2.png)
            run_tui(config)
        }
        Some(Commands::Daemon { command }) => {
            handle_daemon_command(command, config)
        }
        Some(Commands::Plan { task }) => {
            handle_plan_command(task, config)
        }
        Some(Commands::List { status, loop_type }) => {
            handle_list_command(status.as_deref(), loop_type.as_deref(), config)
        }
        // ... other commands
    }
}

/// Initialize and run the TUI per docs/tui.md specification
fn run_tui(config: &Config) -> Result<()> {
    // 1. Enable raw mode
    crossterm::terminal::enable_raw_mode()?;

    // 2. Setup terminal with alternate screen
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // 3. Create app state
    let mut app = loopr::tui::App::with_defaults();

    // 4. Run event loop
    let result = run_event_loop(&mut terminal, &mut app);

    // 5. Restore terminal (always, even on error)
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;

    result
}
```

**The TUI spec includes mockup images:**
- `docs/tui-image1.png` - Chat view layout
- `docs/tui-image2.png` - Loops view layout

These show exactly what must be rendered.

#### 5. Fix `src/cli/commands.rs`

Remove `#[allow(dead_code)]` from `impl Cli` and `impl DaemonCommands` by ensuring main.rs actually calls these methods.

#### 6. Fix `src/daemon/scheduler.rs`

**BEFORE:**
```rust
pub fn can_run(&self, _loop_record: &Loop, parent: Option<&Loop>) -> bool {
```

**AFTER (use the parameter):**
```rust
pub fn can_run(&self, loop_record: &Loop, parent: Option<&Loop>) -> bool {
    // Check loop's own status
    if loop_record.status != LoopStatus::Pending {
        return false;
    }
    // ... rest of logic
```

#### 7. Fix `src/ipc/server.rs`

**BEFORE:**
```rust
struct ClientState {
    #[allow(dead_code)]
    id: u64,
    subscribed: bool,
}
```

**AFTER (use it in logging/debugging):**
```rust
struct ClientState {
    id: u64,
    subscribed: bool,
}

// Then actually use id in log statements:
tracing::debug!(client_id = state.id, "Client connected");
```

#### 8. Fix `src/artifact/spec.rs`

Delete `try_parse_numbered_item()` if it's truly unused, or make `parse_spec_phases()` use it.

#### 9. Fix `src/llm/client.rs` (MockLlmClient)

The `_request` parameter in mock is acceptable for test code, but we should use it:

```rust
async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
    // Log the request for debugging
    tracing::debug!(?request, "MockLlmClient::complete called");
    // ... return queued response
}
```

#### 10. Fix Documentation Inconsistencies

**Root Cause:** The `LoopRunner` problem is a symptom of a larger issue: **documentation inconsistency**. `implementation-phases.md` specified creating a `LoopRunner` struct, while `domain-types.md` explicitly said `LoopRunner` should not exist. The implementation followed one doc, not realizing it contradicted another.

**This is a general mandate, not just a LoopRunner fix.**

### Documentation Consistency Principles

All documentation in this project MUST adhere to these principles:

1. **Internal Consistency:** Documents must not contradict each other. If `domain-types.md` says X and `implementation-phases.md` says Y, that's a bug in the docs.

2. **Code-Doc Alignment:** Documentation must accurately reflect what the code does. If the code diverges from the docs, either the code or the docs must be fixed—never leave them misaligned.

3. **Single Source of Truth:** Each concept has ONE authoritative document:
   - **Type design:** `domain-types.md`
   - **Architecture:** `architecture.md`
   - **Build phases:** `implementation-phases.md` (but must defer to domain-types.md for type design)
   - **TUI spec:** `tui.md`

4. **No Stale Documentation:** When code changes, docs change in the same PR. Dead docs are as bad as dead code.

### Immediate LoopRunner Fixes

1. **`docs/implementation-phases.md`** - Remove all references to `LoopRunner`:
   - Delete any phases that create `LoopRunner` or `LoopRunnerConfig`
   - Replace with instructions to implement `Loop::run()` method
   - Update any code examples showing `LoopRunner` usage

2. **`docs/domain-types.md`** - Verify it remains the authoritative source:
   - Ensure the "LoopRunner was unnecessary" section is prominent
   - Add a warning box or note that `Loop` is self-contained with `run()`

3. **`docs/loop.md`** - Ensure consistency with domain-types.md:
   - Verify `impl Loop { fn run() }` is clearly shown
   - Remove any lingering references to `LoopRunner` if present

4. **`docs/architecture.md`** (if exists) - Update diagrams:
   - Show `Loop` → `LoopManager` flow
   - Remove `LoopRunner` from any architecture diagrams

### Full Documentation Audit

Beyond the LoopRunner case, audit ALL docs for:

- **Cross-references that contradict:** Find any place where Doc A says one thing and Doc B says another
- **Outdated code examples:** Code snippets that no longer compile or reflect current APIs
- **Described-but-not-implemented features:** Docs describing features the code doesn't have
- **Implemented-but-not-documented features:** Code that works but isn't in the docs
- **Inconsistent terminology:** Same concept called different names in different docs

### Documentation README

Create or update `docs/README.md` with:

```markdown
# Documentation Guidelines

## Consistency is Mandatory

Documentation inconsistency caused real bugs in this project (see: LoopRunner incident).
All docs MUST be:

1. **Internally consistent** - Docs must not contradict each other
2. **Code-aligned** - Docs must match what the code actually does
3. **Current** - Update docs in the same PR as code changes

## Single Source of Truth

| Concept | Authoritative Document |
|---------|----------------------|
| Type design & domain model | `domain-types.md` |
| System architecture | `architecture.md` |
| Build/implementation phases | `implementation-phases.md` |
| TUI specification | `tui.md` |
| CLI commands | `cli.md` |

When documents conflict, the authoritative document wins.

## Before Implementing

1. Read `domain-types.md` first—it defines the core abstractions
2. Cross-check `implementation-phases.md` against `domain-types.md`
3. If they conflict, `domain-types.md` is correct—fix the other doc or ask
```

### Implementation Plan

**Phase 1: Delete LoopRunner, add Loop::run()**
- Delete `src/runner/loop_runner.rs`
- Add `run()` method to `Loop` in `src/domain/loop_record.rs`
- Move `LoopOutcome` to `src/domain/outcome.rs`
- Update `src/runner/mod.rs` or delete it

**Phase 2: Wire up LoopManager**
- Remove `#[allow(dead_code)]` from fields
- Fix `start_loop()` to actually use `llm_client` and `tool_router`
- Call `loop_instance.run()` instead of placeholder

**Phase 3: Wire up main.rs**
- Handle all CLI subcommands
- Remove `_cli` underscore prefix
- Add stub handlers that at least print "not implemented" rather than being dead code

**Phase 4: Clean up remaining dead code**
- Fix `scheduler.rs` `_loop_record` parameter
- Fix `ipc/server.rs` `id` field
- Delete or use `try_parse_numbered_item()`
- Review all underscore params in test code

**Phase 5: Fix Documentation Inconsistencies (LoopRunner)**
- Update `implementation-phases.md` to remove all `LoopRunner` references
- Verify `domain-types.md` clearly states `Loop` is self-contained
- Check `loop.md` shows `impl Loop { fn run() }` pattern
- Ensure all docs consistently describe the `Loop` → `LoopManager` architecture (no `LoopRunner`)

**Phase 6: Full Documentation Audit & Consistency Mandate**
- Create/update `docs/README.md` with documentation consistency guidelines
- Define single source of truth table (which doc is authoritative for what)
- Audit ALL docs for:
  - Cross-document contradictions
  - Outdated code examples that don't compile or reflect current APIs
  - Described-but-not-implemented features
  - Implemented-but-not-documented features
  - Inconsistent terminology across docs
- Fix all inconsistencies found
- Ensure every doc accurately reflects current code behavior

**Phase 7: Verify**
- `cargo build` with no `#[allow(dead_code)]`
- `cargo clippy` should show no unused warnings
- `grep -r "allow(dead_code)" src/` returns empty
- `grep -r "let _[a-z]" src/` returns only test code (acceptable)
- `grep -ri "looprunner" docs/` returns nothing (except this design doc explaining the removal)
- Manual review: docs match code, no contradictions between docs

## Alternatives Considered

### Alternative 1: Keep LoopRunner, Just Wire It Up

- **Description:** Leave LoopRunner as-is, wire it into LoopManager
- **Pros:** Less code churn
- **Cons:** Violates domain-types.md design, adds unnecessary indirection
- **Why not chosen:** The docs explicitly say LoopRunner is wrong

### Alternative 2: Delete All Unused Code Without Wiring

- **Description:** Just delete everything marked dead_code
- **Pros:** Fastest to implement
- **Cons:** Loses functionality that should work
- **Why not chosen:** We want the code to work, not just compile

### Alternative 3: Leave As-Is, Fix Docs

- **Description:** Keep LoopRunner, update domain-types.md to match implementation
- **Pros:** No code changes needed
- **Cons:** The design doc was right—LoopRunner IS unnecessary indirection
- **Why not chosen:** The original design reasoning is sound

### Alternative 4: Fix Only LoopRunner Docs, Skip Full Audit

- **Description:** Fix the specific LoopRunner documentation inconsistency but don't audit all docs
- **Pros:** Faster, less scope
- **Cons:** Other inconsistencies likely exist and will cause future problems
- **Why not chosen:** The LoopRunner incident proves documentation inconsistency is a systemic issue, not an isolated case. A full audit and establishing consistency as a project mandate prevents recurrence.

## Technical Considerations

### Dependencies

- `Loop::run()` needs `LlmClient`, `ToolRouter`, `Storage` traits
- These are already defined and working
- No new dependencies needed

### Performance

No performance impact—same code, different organization.

### Security

No security implications—internal refactoring only.

### Testing Strategy

1. **Migrate LoopRunner tests to Loop tests:**
   - `test_loop_runner_run_passes_first_try` → `test_loop_run_passes_first_try`
   - `test_loop_runner_run_max_iterations` → `test_loop_run_max_iterations`
   - `test_loop_runner_accumulates_progress` → `test_loop_run_accumulates_progress`
   - Mock objects (`MockToolRouter`, `MockValidator`) stay in test modules
2. **Run full test suite after each phase:** `cargo test`
3. **Run clippy with deny warnings:** `cargo clippy -- -D warnings`
4. **Verify cleanup complete:**
   ```bash
   grep -r "allow(dead_code)" src/  # Should return nothing
   grep -rE "fn [a-z_]+\([^)]*_[a-z]" src/ | grep -v test  # Minimal results
   ```

### Rollout Plan

Single PR with all changes. No feature flags needed—this is internal refactoring.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Breaking existing tests | Medium | Medium | Run tests after each phase |
| Missing a dead code marker | Low | Low | Use grep to verify |
| LoopRunner tests don't migrate cleanly | Medium | Low | Tests are well-structured, should adapt easily |
| TUI doesn't match mockup images | Medium | Medium | Reference tui-image1.png and tui-image2.png during implementation |
| Event loop missing key handlers | Medium | Medium | Follow keyboard shortcuts from tui.md exactly |
| **Doc inconsistency recurs** | Medium | **High** | Establish doc consistency as project mandate; create `docs/README.md` with guidelines |
| **Docs drift from code over time** | High | **High** | Mandate: docs update in same PR as code changes; no stale docs |

## Edge Cases to Handle

1. **TUI without daemon running:** Should show "Connecting to daemon..." or error gracefully
2. **Daemon without TUI:** Daemon commands should work standalone (`loopr daemon start`)
3. **Loop::run() failure mid-iteration:** Must update storage status to Failed
4. **Validator not provided to LoopManager:** Should fail at construction, not runtime
5. **Empty loops list:** Loops view should show "No loops" not crash

## Open Questions

- [x] Should `LoopOutcome` stay in `src/runner/` or move to `src/domain/`?
  - **Answer:** Move to domain—it's a domain concept
- [x] Should we delete the entire `src/runner/` module or keep it for future runner-subprocess work?
  - **Answer:** Keep `src/runner/` but repurpose it. The docs describe "runner subprocesses" for tool execution (runner-no-net, runner-net, runner-heavy)—these are different from `LoopRunner`. The module can later hold subprocess runner code. For now, it will be minimal or empty.

## Files Affected (Quick Reference)

| File | Action | Reason |
|------|--------|--------|
| `src/runner/loop_runner.rs` | **DELETE** | LoopRunner shouldn't exist |
| `src/runner/mod.rs` | Modify | Remove LoopRunner exports, keep LoopOutcome |
| `src/domain/loop_record.rs` | Modify | Add `run()` method from LoopRunner |
| `src/domain/mod.rs` | Modify | Export LoopOutcome if moved here |
| `src/manager/loop_manager.rs` | Modify | Remove `#[allow(dead_code)]`, wire up `start_loop()` |
| `src/main.rs` | **REWRITE** | Handle CLI commands, start TUI/daemon |
| `src/cli/commands.rs` | Modify | Remove `#[allow(dead_code)]` |
| `src/daemon/scheduler.rs` | Modify | Use `loop_record` parameter |
| `src/ipc/server.rs` | Modify | Use `id` field in logging |
| `src/artifact/spec.rs` | Modify | Delete or use `try_parse_numbered_item()` |
| `src/llm/client.rs` | Modify | Use `request` param in MockLlmClient |
| `docs/implementation-phases.md` | **Modify** | Remove all `LoopRunner` references—this doc caused the problem |
| `docs/domain-types.md` | Verify | Ensure it clearly states `Loop` is self-contained |
| `docs/loop.md` | Verify | Ensure `impl Loop { fn run() }` is shown |
| `docs/README.md` | **Create** | Documentation consistency guidelines & single source of truth |
| `docs/*.md` (all) | **Audit** | Full audit for consistency—docs must match code and each other |

## References

- [domain-types.md](../domain-types.md) - **The authoritative design** (says no LoopRunner) — this is the source of truth
- [implementation-phases.md](../implementation-phases.md) - The build guide (**incorrectly specifies LoopRunner** — must be fixed)
- [loop.md](../loop.md) - Shows `impl Loop { fn run() }` as the design
- [tui.md](../tui.md) - TUI specification with state machine and view layouts
- [tui-image1.png](../tui-image1.png) - Chat view mockup
- [tui-image2.png](../tui-image2.png) - Loops view mockup
