# Design Document: Decompose the Handlers Monolith

**Author:** Scott A. Idler
**Date:** 2026-03-31
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Split `src/daemon/handlers.rs` (13,598 lines, ~85 handler functions, ~570 tests) into a `handlers/` module directory with domain-grouped submodules. The public API stays the same - only `dispatch()` is exported - so all external call sites are unaffected.

## Problem Statement

### Background

The daemon's IPC layer routes every RPC method through a single `dispatch()` function in `handlers.rs`. As Loopr grew from a handful of CRUD operations to 85+ handlers spanning 11 domains (plan, spec, phase, work, bundle, tick, learning, lock, worktree, agent, chat, coordinator, integrator/validator, system), this file accumulated to 13,598 lines. The codebase evaluation (2026-03-21) flagged it as the #2 maintainability concern.

### Problem

- **Navigability**: Finding a specific handler requires scrolling through 5,500+ lines of production code.
- **Merge conflicts**: Any two features touching different handler domains will conflict on the same file.
- **Cognitive load**: A developer reading `handle_tick_create` must mentally filter out 80+ unrelated functions.
- **Test locality**: 8,000 lines of tests are disconnected from the 300-line domain they exercise, making it hard to verify coverage by inspection.
- **LLM context exhaustion**: When Loopr's own agents (or Claude Code) need to work on handler logic, they must read the entire 13K file or use surgical line offsets. A ~300-line `chat.rs` fits easily in any context window. For a project that IS an agent orchestrator, this is especially self-defeating.

### Goals

- Decompose `handlers.rs` into semantically grouped submodules under `handlers/`
- Preserve the single public API: `handlers::dispatch()`
- Co-locate each domain's tests with its handler code
- Zero behavioral changes - pure structural refactor
- `otto ci` passes before and after

### Non-Goals

- Changing handler signatures or logic
- Refactoring the `Stores` type or dispatch mechanism
- Adding new handlers or removing existing ones
- Changing the IPC protocol
- Addressing the `.unwrap()` audit (separate work item)

## Proposed Solution

### Overview

Convert `handlers.rs` into a `handlers/` module directory. Each domain gets its own single-word file. Shared utilities (the `try_handler!` macro, `check_validation_gate`, `auto_start_agents`, etc.) live in a shared file re-exported by `mod.rs`. The `dispatch()` function stays in `mod.rs` and calls into submodule functions via `use` imports.

### Module Structure

```
src/daemon/handlers/
  mod.rs          # dispatch(), auto_start_agents(), max_pool_for(), try_handler! macro, re-exports
  common.rs       # check_validation_gate(), shared utilities
  system.rs       # handshake, system_init, status, shutdown
  plan.rs         # plan create/get/list/transition/update
  spec.rs         # spec create/get/list/transition/update
  phase.rs        # phase create/get/list/transition/update
  work.rs         # work create/get/list/transition/update, detect_dependency_cycle()
  bundle.rs       # bundle create/get/list/transition/update, find_latest_published_tick()
  tick.rs         # tick create/get/list/transition/update
  learning.rs     # learning create/get/list/update/reinforce/contradict/promote/demote
  lock.rs         # lock create/get/list/release/expire
  worktree.rs     # worktree create/list/cleanup/refresh
  integrator.rs   # integrator validate/publish, validator validate/report/reports, coverage evaluate
  coordinator.rs  # coordinator set_goal/clear_goal/get_goal/get_state/reset/interview/accept (calls dispatch() for accept_plan)
  agent.rs        # agent start/stop/pause/resume/status/list/output
  chat.rs         # chat submit/attach/history, build_orchestration_status()
```

16 files replacing 1. Each production file is 200-600 lines. Each carries its own `#[cfg(test)] mod tests`.

### Shared Utilities Placement

| Utility | Used By | Location |
|---------|---------|----------|
| `try_handler!` macro | All handlers | `mod.rs` (macros must be defined before use in sibling modules) |
| `max_pool_for()` | `agent.rs`, `mod.rs` (auto_start) | `mod.rs` |
| `check_validation_gate()` | `plan.rs`, `spec.rs`, `phase.rs` | `common.rs` |
| `auto_start_agents()` | `mod.rs` (dispatch) | `mod.rs` |
| `detect_dependency_cycle()` | `work.rs` only | `work.rs` |
| `find_latest_published_tick()` | `bundle.rs` only | `bundle.rs` |
| `run_validation_commands()` | `integrator.rs` only | `integrator.rs` |
| `get_git_head_sha()` | `integrator.rs` only | `integrator.rs` |
| `build_orchestration_status()` | `chat.rs` only | `chat.rs` |
| `dispatch()` (recursive call) | `coordinator.rs` (`accept_plan`) | `mod.rs` - coordinator calls `super::dispatch()` |

### Visibility Strategy

All handler functions are `pub(super)` - visible within `daemon::handlers` but not outside. Only `dispatch()` is `pub` (the existing contract). The `try_handler!` macro is defined in `mod.rs` before the `mod` declarations - Rust's macro scoping makes it visible to all child modules declared after it. No `#[macro_export]` needed.

### Test Migration

Each domain's tests move into that domain's file. Shared test helpers (`test_stores()`, `test_stores_with_taskstore()`, `test_stores_with_validator_strictness()`, `test_event_tx()`, `test_worktree_mgr()`, `test_integrator_config()`) stay in a `#[cfg(test)] pub(crate) mod tests` block in `mod.rs`. Submodule tests import them via:

```rust
use crate::daemon::handlers::tests::{test_stores, test_event_tx, test_worktree_mgr};
```

Using `pub(crate)` on the test module (rather than `pub(super)`) avoids path gymnastics from nested test modules. The `#[cfg(test)]` gate ensures these helpers are stripped from release builds.

Tests that exercise `dispatch()` directly (e.g., `test_dispatch_unknown_method`, `test_dispatch_handshake`, `test_dispatch_status_*`, `test_dispatch_shutdown`) stay in `mod.rs` since they test the routing layer itself, not any specific domain.

### Concrete Example: `system.rs`

This is what an extracted domain file looks like:

```rust
// src/daemon/handlers/system.rs

use std::sync::Arc;

use log::debug;
use serde_json::json;
use tokio::sync::broadcast;

use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use crate::daemon::context::Stores;

pub(super) fn handle_handshake(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    // ... body unchanged ...
}

pub(super) fn handle_system_init(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    // ... body unchanged ...
}

// ... remaining system handlers ...

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::handlers::tests::{test_stores, test_event_tx};  // shared helpers from mod.rs
    use serde_json::json;

    #[test]
    fn test_handshake() {
        // ... test body unchanged ...
    }
}
```

Key points:
- `pub(super)` on each handler function (was module-private, now needs to be visible to parent `mod.rs`)
- Imports use `crate::daemon::context::Stores` (not `super::context` - after the split, `super` from `system.rs` is `handlers`, not `daemon`)
- Test helpers imported from `crate::daemon::handlers::tests::` (the shared test module in `mod.rs`)
- `try_handler!` macro is inherited from parent `mod.rs` - no import needed

### dispatch() Routing

The `dispatch()` function in `mod.rs` stays structurally identical - a big `match` on `req.method.as_str()`. The only change is that handler function names are now qualified or imported:

```rust
// mod.rs
mod agent;
mod bundle;
mod chat;
mod common;
mod coordinator;
mod integrator;
mod learning;
mod lock;
mod phase;
mod plan;
mod spec;
mod system;
mod tick;
mod work;
mod worktree;

use agent::*;
use bundle::*;
use chat::*;
use coordinator::*;
use integrator::*;
use learning::*;
use lock::*;
use phase::*;
use plan::*;
use spec::*;
use system::*;
use tick::*;
use work::*;
use worktree::*;

// dispatch() body unchanged
```

### Implementation Plan

**Phase 1: Scaffold the module directory**
- Create `src/daemon/handlers/` directory
- Move `handlers.rs` content into `handlers/mod.rs`
- Verify `otto ci` passes (zero-diff refactor)

**Phase 2: Extract shared utilities**
- Move `check_validation_gate()` to `common.rs`
- Keep `try_handler!`, `max_pool_for()`, `auto_start_agents()` in `mod.rs`
- Verify `otto ci` passes

**Phase 3: Extract domain handlers (one at a time)**
- Extract in dependency order to keep the build green at each step:
  1. `system.rs` (no cross-deps)
  2. `lock.rs` (no cross-deps)
  3. `learning.rs` (no cross-deps)
  4. `worktree.rs` (no cross-deps)
  5. `tick.rs` (no cross-deps)
  6. `plan.rs` (depends on `common::check_validation_gate`)
  7. `spec.rs` (depends on `common::check_validation_gate`)
  8. `phase.rs` (depends on `common::check_validation_gate`)
  9. `work.rs` (self-contained `detect_dependency_cycle`)
  10. `bundle.rs` (self-contained `find_latest_published_tick`)
  11. `integrator.rs` (self-contained `run_validation_commands`, `get_git_head_sha`)
  12. `coordinator.rs` (accept_plan calls `super::dispatch()` recursively - must be extracted after dispatch is stable in mod.rs)
  13. `agent.rs` (uses `max_pool_for` from mod.rs)
  14. `chat.rs` (self-contained `build_orchestration_status`)
- Each extraction: move handlers + their tests, verify `otto ci`

**Phase 4: Cleanup**
- Remove any dead imports from `mod.rs`
- Verify final `otto ci`
- Verify external references (`bridge.rs`, `supervisor.rs`, `fsm_correctness_tests.rs`, `integration_tests.rs`) still compile

### Migration Safety

Each step is a standalone commit that passes CI. If any extraction breaks something, the previous commit is the rollback point. The match arms in `dispatch()` don't change - only the source location of the functions they call.

### Known Trade-offs

- **Git blame breaks**: `git blame` on the new files won't show pre-split history. Use `git log --all -- src/daemon/handlers.rs` to trace the original monolith. This is a one-time cost that pays off with cleaner blame going forward.
- **Per-file boilerplate**: Each domain file needs its own `#[allow(clippy::unwrap_used)]` on its test module and its own import block. Duplication is minimal (~5 lines per file) and the alternative (one massive file) is worse.

## Alternatives Considered

### Alternative 1: Trait-based handler dispatch

- **Description:** Define a `Handler` trait, implement per-domain handler structs, use dynamic dispatch.
- **Pros:** More extensible, cleaner separation.
- **Cons:** Significant refactor beyond structural. Introduces `dyn` dispatch overhead. Changes the handler calling convention. Higher risk for a pure maintainability improvement.
- **Why not chosen:** Violates the non-goal of changing handler signatures. The current `match`-based dispatch is clear and fast. This is a maintainability refactor, not an architecture change.

### Alternative 2: Keep single file, use code folding

- **Description:** Add `// region:` markers and rely on IDE folding.
- **Pros:** Zero code changes.
- **Cons:** Doesn't fix merge conflicts. Doesn't fix test locality. Band-aid.
- **Why not chosen:** Doesn't address the actual problems.

### Alternative 3: Two-level split (domain groups)

- **Description:** Group related domains: `hierarchy.rs` (plan/spec/phase/work), `pipeline.rs` (bundle/tick/integrator), `orchestration.rs` (coordinator/agent/chat).
- **Pros:** Fewer files (5-6 vs 16).
- **Cons:** Each file is still 1,000-2,000 lines. Still have merge conflict risk within groups. Doesn't match the IPC method namespace (`plan.*`, `spec.*`, etc.).
- **Why not chosen:** The IPC method prefixes already define natural module boundaries. One file per prefix is the obvious decomposition.

## Technical Considerations

### Dependencies

- No new external dependencies.
- Internal: Only moves code between files within the same module.

### Performance

- Zero runtime impact. All dispatch is static function calls, unchanged.

### Testing Strategy

- `otto ci` after every extraction step.
- Final validation: `cargo test` with `--nocapture` to verify all 570 tests pass.
- External test files (`fsm_correctness_tests.rs`, `integration_tests.rs`) import `crate::daemon::handlers::dispatch` - this path is unchanged.

### Rollout Plan

- Single branch, merged as one PR.
- Each extraction is a separate commit for bisect-ability.
- No feature flags needed - pure refactor.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Macro visibility across submodules | Medium | Low | Define `try_handler!` in `mod.rs` before submodule declarations; test in Phase 2 |
| Test helper visibility across submodules | Medium | Low | Place shared helpers in `#[cfg(test)]` block in `mod.rs`; submodules use `super::` |
| Missed import in one submodule | Low | Low | Each step verified by `otto ci`; compiler catches immediately |
| Merge conflict with concurrent work on v3 | Low | Medium | Do this on a quiet day or as a dedicated branch |

## Open Questions

- [ ] Should the hierarchy handlers (plan/spec/phase) share a generic implementation? They follow an identical create/get/list/transition/update pattern. This could be a follow-up refactor after decomposition.
- [x] ~~Should test helpers live in `mod.rs#[cfg(test)]` or a dedicated `tests.rs` helper file?~~ Decided: `#[cfg(test)] pub(crate) mod tests` in `mod.rs`. Keeps helpers co-located with `dispatch()` tests and avoids an extra file.

## References

- [next-steps.md #5](../next-steps.md) - Roadmap item
- [2026-03-21-codebase-evaluation.md](../2026-03-21-codebase-evaluation.md) - Assessment flagging this as concern #2
- [Rust naming conventions](../../CLAUDE.md) - Single-word filenames, module decomposition rules
