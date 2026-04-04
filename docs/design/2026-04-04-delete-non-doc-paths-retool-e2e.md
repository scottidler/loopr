# Design Document: Delete Non-Doc Plan Entry Paths and Retool E2E Targets

**Author:** Scott A. Idler
**Date:** 2026-04-04
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The codebase has three plan entry paths: `doc.accept` (chat funnel), `doc.inject` (E2E injection), and three legacy paths (`coordinator.set_goal` + `coordinator.accept_plan`, `coordinator.seed_manifest`, and an inline heredoc in `rust-version.sh`). The authoritative doc `docs/2026-04-04-chat-tunnel-vs-e2e-insertion-for-entering-a-plan.md` mandates exactly two paths. This document describes what to delete and how to retool the E2E target scripts to use the correct path.

---

## Problem Statement

### Background

The Doc architecture introduced `doc.accept` and `doc.inject` as the two authoritative plan entry paths. Both routes call `accept_plan_markdown()`, which runs the Decomposer and hands off to the Coordinator. These paths were added during the v3 architecture migration and are fully implemented.

The legacy paths pre-date the Doc architecture:
- `coordinator.set_goal` - sets a text goal, no plan or decomposition
- `coordinator.clear_goal` - clears the goal
- `coordinator.accept_plan` - accepts inline text or a plan ID, bypasses Decomposer
- `coordinator.seed_manifest` - loads a YAML manifest and bypasses Decomposer entirely

All four legacy handlers are in `src/daemon/handlers/coordinator.rs`. Their CLI surface is in `src/cli.rs` (`CoordinatorCmd` variants) and `src/cli/dispatch.rs` (`run_headless`). The TUI still has `IpcAction::SetGoal` and `IpcAction::AcceptPlan` variants. Integration tests use `coordinator.set_goal` for scaffolding.

### Problem

Dead code in active paths causes confusion, drift, and real bugs:
1. `run_headless` routes `.yml`/`.yaml` files through `coordinator.seed_manifest` (bypasses Decomposer)
2. `run_headless` routes `else` text through `coordinator.set_goal` + `coordinator.accept_plan`
3. `src/agents/executor/action/record.rs` auto-calls `coordinator.accept_plan` after `handle_create_plan`
4. The eight E2E target `.sh` files pass `.yml` manifest paths (or inline heredoc text) to `target_plan()` instead of `.md` plan files
5. All of this contradicts the authoritative two-path architecture

### Goals

- Delete all code that implements the `coordinator.set_goal`, `coordinator.clear_goal`, `coordinator.accept_plan`, and `coordinator.seed_manifest` IPC methods
- Delete `src/manifest.rs` entirely
- Collapse `run_headless` to only accept `.md` plan files (passed to `doc.inject`)
- Remove the dead TUI `IpcAction` variants
- Retool all eight E2E target `.sh` files to return a path to a `.md` plan document
- Write the four missing `.md` plan documents (python-api, node-api, rust-cli, python-scraper)
- Migrate integration tests that used `coordinator.set_goal` as scaffolding to use a direct store insertion helper

### Non-Goals

- Implementing `doc.accept` or `doc.inject` (already implemented)
- Changing how the Decomposer or Coordinator work
- Modifying the `coordinator.get_goal` handler (keep it - it reads state, costs nothing)
- Changing `target_goal()` in any `.sh` file
- Changing `scaffold()`, `collect_results()`, or `verify()` in any `.sh` file
- Changing `target_validation_commands()` in any `.sh` file

---

## Proposed Solution

### Overview

Two parallel tracks of work:

**Track A - Source code deletion**: Remove all legacy IPC handlers, CLI commands, TUI actions, and `src/manifest.rs`. Replace integration test scaffolding that used `coordinator.set_goal` with a direct `seed_goal()` helper. Each deletion must leave `otto ci` green.

**Track B - E2E retooling**: Write four missing `.md` plan files, then update eight `.sh` files to point `target_plan()` at the corresponding `.md` file. No source code changes.

These tracks are independent and can be executed in parallel or sequentially.

---

### Architecture

After this change, the plan entry architecture is exactly:

```
                    TUI Chat                     E2E Test
                       |                            |
              User types /accept            doc.inject(path)
                       |                            |
               doc.accept(markdown)                 |
                       |                            |
               accept_plan_markdown() <-------------+
                       |
              Decomposer runs
                       |
              Coordinator executes
```

No other entry point exists.

---

### Implementation Plan

#### Phase 1 - Write missing `.md` plan files (E2E, no src changes)

Four files to write, following the plan template in `docs/templates/plan.md`:

| File | Target |
|------|--------|
| `bin/e2e-targets/python-api.md` | FastAPI + SQLite bookmarks REST API |
| `bin/e2e-targets/node-api.md` | Express + SQLite notes REST API |
| `bin/e2e-targets/rust-cli.md` | Rust notes CLI with clap + rusqlite |
| `bin/e2e-targets/python-scraper.md` | Python HTML link harvester with Docker |

Each `.md` must include: Problem Statement, Goals, Requirements table, Contracts (data model + CLI/API), Acceptance Criteria table, Specs section. The four already written (`rust-version.md`, `python-todo.md`, `lua-todo.md`, `react-todo.md`) serve as templates.

#### Phase 2 - Update `target_plan()` in eight `.sh` files (E2E, no src changes)

| File | Current `target_plan()` returns | After |
|------|---------------------------------|-------|
| `rust-version.sh` | inline heredoc text | `${LOOPR_ROOT}/bin/e2e-targets/rust-version.md` |
| `react-todo.sh` | `${LOOPR_ROOT}/bin/e2e-targets/react-todo.yml` | `${LOOPR_ROOT}/bin/e2e-targets/react-todo.md` |
| `python-todo.sh` | `${LOOPR_ROOT}/bin/e2e-targets/python-todo.yml` | `${LOOPR_ROOT}/bin/e2e-targets/python-todo.md` |
| `lua-todo.sh` | `${LOOPR_ROOT}/bin/e2e-targets/lua-todo.yml` | `${LOOPR_ROOT}/bin/e2e-targets/lua-todo.md` |
| `python-api.sh` | `${LOOPR_ROOT}/bin/e2e-targets/python-api.yml` | `${LOOPR_ROOT}/bin/e2e-targets/python-api.md` |
| `node-api.sh` | `${LOOPR_ROOT}/bin/e2e-targets/node-api.yml` | `${LOOPR_ROOT}/bin/e2e-targets/node-api.md` |
| `rust-cli.sh` | `${LOOPR_ROOT}/bin/e2e-targets/rust-cli.yml` | `${LOOPR_ROOT}/bin/e2e-targets/rust-cli.md` |
| `python-scraper.sh` | `${LOOPR_ROOT}/bin/e2e-targets/python-scraper.yml` | `${LOOPR_ROOT}/bin/e2e-targets/python-scraper.md` |

Also remove stale comments inside `target_validation_commands()` functions that reference `.yml` files (e.g. "Phase-scoped validation-commands in react-todo.yml handle this"). The function behavior does not change; only the stale comment is removed.

Delete the seven committed `.yml` manifest files - they will no longer be referenced:
- `bin/e2e-targets/python-todo.yml`
- `bin/e2e-targets/react-todo.yml`
- `bin/e2e-targets/lua-todo.yml`
- `bin/e2e-targets/python-api.yml`
- `bin/e2e-targets/node-api.yml`
- `bin/e2e-targets/rust-cli.yml`
- `bin/e2e-targets/python-scraper.yml`

(Use `rkvr rmrf` per safety rules - no bare `rm`.)

#### Phase 3 - Delete `src/manifest.rs` and deregister it

1. Delete `src/manifest.rs`
2. Remove `pub mod manifest;` from `src/lib.rs`

Verify: `cargo check` passes.

#### Phase 4 - Delete legacy IPC handlers

From `src/daemon/handlers/coordinator.rs`, delete:
- Function `handle_coordinator_set_goal` (and its unit tests)
- Function `handle_coordinator_clear_goal` (and its unit tests)
- Function `handle_coordinator_accept_plan` (and its unit tests)
- Function `handle_coordinator_seed_manifest` (and its unit tests)
- Helper `create_manifest_docs`
- All now-unused imports: `Plan`, `HierarchyStatus`, `Doc`, `DocKind`, `create_run_dir`, `write_doc_file`, `persist_doc`, `IntegratorConfig`, `WorktreeManager`

From `src/daemon/handlers.rs` routing table, remove:
```
"coordinator.set_goal" => handle_coordinator_set_goal(...)
"coordinator.clear_goal" => handle_coordinator_clear_goal(...)
"coordinator.accept_plan" => handle_coordinator_accept_plan(...)
"coordinator.seed_manifest" => handle_coordinator_seed_manifest(...)
```

Verify: `cargo check` passes.

#### Phase 5+6 - Delete CLI command variants AND collapse `run_headless` (must be done together)

**WARNING: Phases 5 and 6 cannot be applied separately.** Removing `CoordinatorCmd::Set/Clear/AcceptPlan` from `src/cli.rs` causes a compile error in `coordinator_to_ipc()` in `src/cli/dispatch.rs` until those match arms are also deleted. Do both in a single edit pass.

From `src/cli.rs`, `CoordinatorCmd` enum, remove:
- `Set { goal: String }` variant
- `Clear` variant
- `AcceptPlan { plan: String }` variant

Keep: `Status` variant (maps to `coordinator.get_goal`).

Remove associated CLI tests from `src/cli.rs`: `test_cli_parses_coordinator_set_goal`, `test_cli_parses_coordinator_clear_goal`, `test_cli_parses_coordinator_accept_plan`.

From `src/cli/dispatch.rs`:
- In `run_headless()`: delete the `is_manifest` boolean, the `.yml`/`.yaml` detection, and both branches. Replace with a single path: if `plan_text` is `Some(path)` ending in `.md`, call `doc.inject` with `{ "path": path }` (the file path, not text content). If no `--plan` is provided or it does not end in `.md`, return an error telling the caller a `.md` plan file is required.
- The `goal` positional argument is currently forwarded to `coordinator.set_goal`. After deletion, goal title comes from the plan `.md` `# ` heading. Keep `goal` as a positional argument accepted at the CLI level (the `bin/e2e` script still passes `"${GOAL}"`) but treat it as a display-only hint; do not forward it to any IPC call.
- In `coordinator_to_ipc()`: delete `CoordinatorCmd::Set`, `CoordinatorCmd::Clear`, `CoordinatorCmd::AcceptPlan` arms.

Remove from `src/cli/dispatch/tests.rs`: `test_coordinator_set_goal_mapping`, `test_coordinator_clear_goal_mapping`, `test_coordinator_accept_plan_mapping`.

Verify: `cargo check` passes.

#### Phase 7 - Remove dead TUI IpcAction variants

From `src/tui/app.rs`:
- Remove `IpcAction::SetGoal(String)` variant
- Remove `IpcAction::AcceptPlan(String)` variant

From `src/tui/run/ipc.rs`:
- Remove `IpcAction::SetGoal(goal)` arm in `dispatch_ipc_action()`
- Remove `IpcAction::AcceptPlan(plan_text)` arm in `dispatch_ipc_action()`
- Remove `test_dispatch_ipc_action_set_goal` test

Verify: `cargo check` passes.

#### Phase 8 - Remove `coordinator.accept_plan` auto-calls from executor

From `src/agents/executor/action/record.rs`:
- Line ~50: remove auto-approve `coordinator.accept_plan` call after `handle_create_plan`
- Line ~190: remove second `coordinator.accept_plan` call

Verify: `cargo check` passes.

#### Phase 9 - Migrate integration test scaffolding

Add to `src/tests/integration/fixtures.rs`:
```rust
pub(super) fn seed_goal(stores: &Arc<Stores>, goal_text: &str) -> String {
    use crate::domain::coordinator_goal::CoordinatorGoal;
    let goal = CoordinatorGoal::new(goal_text.to_string());
    let id = goal.id.clone();
    stores.coordinator_goals.write().unwrap().insert(id.clone(), goal);
    id
}
```

Then apply per-file:

**`src/tests/integration/coordinator.rs`**:
- Delete `test_coordinator_get_goal_full_lifecycle` entirely (tests set_goal/clear_goal lifecycle - both deleted)
- In `test_coordinator_state_persistence_across_iterations`: the current code does:
  ```rust
  let goal = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.set_goal", json!({"goal": "Build a REST API"}));
  let goal_id = goal["id"].as_str().unwrap().to_string();
  ```
  Replace with:
  ```rust
  let goal_id = seed_goal(&stores, "Build a REST API");
  ```
  `seed_goal()` returns `String` directly - no JSON unwrapping needed.

**`src/tests/integration/pipeline.rs`** (three calls at lines ~25, ~231, ~419):
- Each call discards its result. Replace with `seed_goal(&stores, "...");` (semicolon - discard the returned String).

**`src/tests/integration/fsm.rs`** (one call at line ~60):
- Result is bound to `_goal` or discarded. Replace with `seed_goal(&stores, "...");`.

**`src/tests/integration/tick.rs`** (two set_goal calls + one clear_goal call):
- `test_goal_lifecycle`: this test specifically tests the `set_goal`/`clear_goal` IPC lifecycle. It is not scaffolding for another test - it tests the deleted functionality. Delete it entirely. No `seed_goal` replacement needed.

**`src/tests/integration/config.rs`** (`test_dispatch_routes_mvp4_methods`):
- Remove `"coordinator.set_goal"` and `"coordinator.clear_goal"` from the methods list

Verify: `otto ci` passes.

---

## Alternatives Considered

### Alternative 1: Keep `coordinator.seed_manifest` for human-authored hierarchies

**Description:** The authoritative doc mentions the manifest is a "legitimate" third entry for human-authored decompositions. Keep the handler but remove it from `run_headless`.

**Pros:** Preserves the option for pre-decomposed manifests.

**Cons:** Dead code that will never be called from any path. The doc says "it is NOT a substitute for the Decomposer working correctly" - keeping it implies it is. Creates ambiguity that has already caused repeated confusion.

**Why not chosen:** If the Decomposer can't handle a case, fix the Decomposer. Don't keep an escape hatch that contradicts the architecture.

### Alternative 2: Keep `coordinator.set_goal` for integration test scaffolding

**Description:** Rather than adding `seed_goal()`, keep `coordinator.set_goal` as a thin test-only handler.

**Pros:** Less churn in integration tests.

**Cons:** "Test-only" handlers always leak into production. The handler would still be registered in the routing table. The `seed_goal()` helper is six lines and unambiguous.

**Why not chosen:** Dead routes in a routing table are not "test-only" by any definition that matters.

---

## Technical Considerations

### Dependencies

No new dependencies. All deletions.

### Testing Strategy

Each phase ends with `cargo check` at minimum. The final phase ends with `otto ci` (lint + check + test). No phase should leave the build broken.

### Rollout Plan

Phases can be executed in any order within a track. The two tracks (E2E retooling and source deletion) are fully independent. Recommended sequence:

1. Phase 1 (write `.md` files) - pure additions, zero risk
2. Phase 2 (update `.sh` files) - E2E only, zero src risk
3. Phases 3-9 (source deletion) - in order, each verified with `cargo check`
4. Final `otto ci` after Phase 9

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| A deleted handler was still called from a path not yet mapped | Medium | High | `cargo check` will catch it; Grep all usages before deleting |
| Integration test uses `coordinator.set_goal` result shape (JSON) in assertions | Medium | Medium | Only `coordinator.rs:test_coordinator_state_persistence_across_iterations` captures the result; `seed_goal()` returns `String` directly |
| A `.md` plan file is missing a required section | Low | Medium | Check against `docs/templates/plan.md` before committing |
| Phases 5 and 6 applied separately leaves build broken | High | Medium | Apply in a single edit pass |
| `.yml` files referenced from other scripts or CI | Low | Low | `rkvr rmrf` them; `cargo check` and `otto ci` will catch any missed references |
| `goal` positional arg removal breaks `bin/e2e` invocation | Low | High | Keep `goal` as accepted-but-ignored positional in CLI; no change to `bin/e2e` |

---

## Open Questions

- [ ] **`goal` positional arg fate**: Keep `goal` as an accepted-but-ignored positional in `run_headless` so `bin/e2e` does not need to change. The goal title is extracted from the plan `.md` `# ` heading by `accept_plan_markdown`. Log the passed `goal` text for debugging, then discard it.
- [ ] **`coordinator.get_goal` cleanup**: No cleanup needed. `doc.accept`/`doc.inject` create `CoordinatorGoal` records in `accept_plan_markdown`. `get_goal` reads from that store and is independent of `set_goal`.
- [ ] **`doc.rs` comment on line 17-18**: Remove the sentence "The manifest entry path (`coordinator.seed_manifest`) is in coordinator.rs and skips the Decomposer..." when deleting `seed_manifest` in Phase 4.

---

## References

- `docs/2026-04-04-chat-tunnel-vs-e2e-insertion-for-entering-a-plan.md` - authoritative two-path doc
- `docs/templates/plan.md` - required plan document structure
- `bin/e2e-targets/rust-version.md` - example Brief-mode plan
- `bin/e2e-targets/python-todo.md` - example Full-mode plan
