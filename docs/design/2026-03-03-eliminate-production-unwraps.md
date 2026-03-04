# Design Document: Eliminate Production `unwrap()` Calls

**Author:** Scott Idler + Claude
**Date:** 2026-03-03
**Status:** Complete
**Review Passes Completed:** 5/5

## Summary

The codebase has ~379 `.unwrap()` calls in production code. These are ticking time bombs — any poisoned lock or unexpected `None` crashes the daemon. This doc designs a systematic elimination using eyre error propagation, lock helper methods on `Stores`, and a handler-layer `try_handler!` macro.

## Problem Statement

### Background

Loopr uses `eyre` for error handling and `thiserror` for domain error types. The agent layer (coordinator, executor, integrator, etc.) already returns `eyre::Result<T>` from all major methods. The `Agent` trait enforces `async fn run(&mut self) -> Result<()>`.

Despite this, ~379 production `.unwrap()` calls remain — mostly on `RwLock::read()/write()` and `Mutex::lock()` on the `Stores` struct. These were written during rapid MVP development and never cleaned up.

### Problem

A single poisoned lock (caused by a panic in any thread holding that lock) will cascade-crash the daemon via `.unwrap()`. The `Stores` struct has 14 `RwLock` fields and 3 `Mutex` fields — a panic in any lock-holding code path poisons that lock for all future callers. Every subsequent `.unwrap()` on that lock triggers another panic, taking down the entire daemon.

Beyond locks, there are `serde_json::to_value().unwrap()` calls (infallible in practice but wrong in principle), `HashMap::get_mut().unwrap()` calls (logic errors if the key is absent), and scattered `Option::unwrap()` calls that assume presence without proof.

### Goals

- Zero `.unwrap()` calls in production code (test code is fine)
- Lock access via fallible helper methods on `Stores` that return `eyre::Result`
- Handler layer converts internal `Result<T>` into `DaemonResponse` error responses
- Agent layer propagates lock errors via `?` with context
- No behavioral change — pure error handling improvement

### Non-Goals

- Changing lock types (e.g., switching to `parking_lot` — different concern)
- Restructuring the `Stores` struct or handler dispatch architecture
- Adding retry logic for poisoned locks (poisoning = prior panic = already in trouble)
- Touching test code `.unwrap()` calls (those are fine)

## Proposed Solution

### Overview

Three layers need different fixes:

1. **`Stores` helper methods** — typed accessors that return `eyre::Result<Guard>` instead of panicking
2. **Handler layer macro** — `try_handler!` converts `Result<DaemonResponse>` → `DaemonResponse`
3. **Agent layer** — direct `?` replacement since functions already return `Result`

### Layer 1: `Stores` Lock Helpers

Add fallible accessor methods to `Stores` in `src/daemon/context.rs`:

```rust
use std::sync::{RwLockReadGuard, RwLockWriteGuard, MutexGuard};
use eyre::{Result, eyre};

impl Stores {
    // --- RwLock helpers ---

    pub fn read_plans(&self) -> Result<RwLockReadGuard<'_, HashMap<String, Plan>>> {
        self.plans.read().map_err(|_| eyre!("plans lock poisoned"))
    }

    pub fn write_plans(&self) -> Result<RwLockWriteGuard<'_, HashMap<String, Plan>>> {
        self.plans.write().map_err(|_| eyre!("plans lock poisoned"))
    }

    // ... repeat for all 14 RwLock fields: specs, phases, works, bundles,
    //     ticks, learnings, locks, coordinator_goals, coordinator_states,
    //     proposals, decisions, agent_sessions, coverage_reports, agent_events

    // --- Mutex helpers ---

    pub fn lock_store(&self) -> Result<Option<MutexGuard<'_, Store>>> {
        match &self.store {
            Some(s) => Ok(Some(s.lock().map_err(|_| eyre!("taskstore lock poisoned"))?)),
            None => Ok(None),
        }
    }

    pub fn lock_store_required(&self) -> Result<MutexGuard<'_, Store>> {
        let store = self.store.as_ref()
            .ok_or_else(|| eyre!("TaskStore not initialized"))?;
        store.lock().map_err(|_| eyre!("taskstore lock poisoned"))
    }

    pub fn lock_agent_handles(&self) -> Result<MutexGuard<'_, HashMap<String, JoinHandle<()>>>> {
        self.agent_handles.lock().map_err(|_| eyre!("agent_handles lock poisoned"))
    }

    pub fn lock_git(&self) -> Result<MutexGuard<'_, ()>> {
        self.git_lock.lock().map_err(|_| eyre!("git_lock poisoned"))
    }
}
```

A macro reduces boilerplate for the 14 nearly-identical RwLock pairs:

```rust
macro_rules! store_accessors {
    ($($field:ident : $value_type:ty),* $(,)?) => {
        $(
            paste::paste! {
                pub fn [<read_ $field>](&self) -> Result<RwLockReadGuard<'_, HashMap<String, $value_type>>> {
                    self.$field.read().map_err(|_| eyre!(concat!(stringify!($field), " lock poisoned")))
                }
                pub fn [<write_ $field>](&self) -> Result<RwLockWriteGuard<'_, HashMap<String, $value_type>>> {
                    self.$field.write().map_err(|_| eyre!(concat!(stringify!($field), " lock poisoned")))
                }
            }
        )*
    };
}

impl Stores {
    store_accessors! {
        plans: Plan,
        specs: Spec,
        phases: Phase,
        works: Work,
        bundles: Bundle,
        ticks: Tick,
        learnings: Learning,
        locks: Lock,
        coordinator_goals: CoordinatorGoal,
        coordinator_states: CoordinatorState,
        proposals: Proposal,
        decisions: Decision,
        agent_sessions: AgentSession,
        coverage_reports: CoverageReport,
        agent_events: VecDeque<AgentEvent>,
    }
}
```

**Dependency:** `paste` crate for the `paste!` identifier concatenation macro. Add via `cargo add paste`.

### Layer 2: Handler Layer — `try_handler!` Macro

Handlers return `DaemonResponse`, not `Result`. They can't use `?` directly. A macro bridges this:

```rust
/// Convert a handler body that returns Result<DaemonResponse> into a DaemonResponse,
/// mapping any Err into an RPC internal error response.
macro_rules! try_handler {
    ($req_id:expr, $body:expr) => {{
        #[allow(clippy::redundant_closure_call)]
        let __result = (|| -> eyre::Result<DaemonResponse> { $body })();
        match __result {
            Ok(resp) => resp,
            Err(e) => DaemonResponse::err($req_id, RpcError::internal(&e.to_string())),
        }
    }};
}
```

> **Implementation note:** The double-brace `{{ }}` and `#[allow(clippy::redundant_closure_call)]` were added during implementation. Clippy flags the immediately-invoked closure pattern as redundant; the allow is scoped to each macro expansion site.

Before:
```rust
fn handle_status(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let plans = stores.plans.read().unwrap().len();
    let specs = stores.specs.read().unwrap().len();
    // ... 9 more unwraps
    DaemonResponse::ok(req.id, json!({ "counts": { "plans": plans, ... } }))
}
```

After:
```rust
fn handle_status(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        let plans = stores.read_plans()?.len();
        let specs = stores.read_specs()?.len();
        // ... clean ? propagation
        Ok(DaemonResponse::ok(req.id, json!({ "counts": { "plans": plans, ... } })))
    })
}
```

### Layer 3: Agent Layer — Direct `?` Replacement

Agent methods already return `eyre::Result<T>`. Just swap:

Before:
```rust
let works = stores.works.read().unwrap();
```

After:
```rust
let works = stores.read_works()?;
```

For `serde_json::to_value()`:

Before:
```rust
let json = serde_json::to_value(&report).unwrap();
```

After:
```rust
let json = serde_json::to_value(&report)?;
```

For `HashMap` gets:

Before:
```rust
bundles.get_mut(&id).unwrap()
```

After:
```rust
bundles.get_mut(&id).ok_or_else(|| eyre!("bundle not found: {id}"))?
```

### Layer 4: Free Functions

Functions like `record_agent_event()` currently return `()` and unwrap internally. Two options:

1. **Change signature to `Result<()>`** — callers propagate or log
2. **Log-and-continue** — for fire-and-forget operations

`record_agent_event` is fire-and-forget, so:

```rust
pub fn record_agent_event(stores: &Stores, session_id: &str, event: &AgentEvent) {
    let Ok(mut events) = stores.write_agent_events() else {
        log::error!("agent_events lock poisoned, dropping event for session {session_id}");
        return;
    };
    // ... rest unchanged
}
```

### Implementation Plan

#### Phase 1: Infrastructure (Stores helpers + macro) — COMPLETE

**Commit:** `0881ad4`
**Files:** `src/daemon/context.rs`, `src/daemon/handlers.rs`

1. Added `paste` dependency: `cargo add paste`
2. Added `store_accessors!` macro generating `read_*`/`write_*` methods for all 14 RwLock fields
3. Added Mutex helpers: `lock_store`, `lock_store_required`, `lock_agent_handles`, `lock_git`
4. Added `try_handler!` macro to `src/daemon/handlers.rs`
5. Fixed `record_agent_event` to use `let-else` pattern (fire-and-forget)
6. `otto ci` green, all 1843 tests pass

#### Phase 2: Handlers (`handlers.rs`) — COMPLETE

**Commit:** `843e238`
**Files:** `src/daemon/handlers.rs`

Eliminated all 194 production unwraps in the largest file:

1. Wrapped every handler body in `try_handler!(req.id, { ... })`
2. Replaced `stores.<field>.read().unwrap()` → `stores.read_<field>()?`
3. Replaced `stores.<field>.write().unwrap()` → `stores.write_<field>()?`
4. Replaced `store.lock().unwrap()` → `stores.lock_store_required()?`
5. Replaced `serde_json::to_value().unwrap()` → `serde_json::to_value()?`
6. Replaced `HashMap::get_mut().unwrap()` → `.ok_or_else(|| eyre!(...))?`
7. Fixed `find_latest_published_tick` to use `stores.read_ticks().ok()?` (returns `Option`)
8. Fixed handler-to-handler call in `handle_integrator_seal` to wrap in `Ok(...)`
9. Added `#[allow(clippy::redundant_closure_call)]` to `try_handler!` macro
10. `otto ci` green, all 1843 tests pass

#### Phase 3: Agent Layer — COMPLETE

**Files:** `src/agents/coordinator.rs`, `src/agents/executor.rs`, `src/agents/integrator.rs`, `src/agents/generation.rs`, `src/agents/worker.rs`, `src/agents/mod.rs`, `src/agents/implementer.rs`, `src/agents/context.rs`

Eliminated ~117 production unwraps across agent files. Different patterns used based on function return type:

- **`Result`-returning functions:** `stores.read_field()?`
- **`Option`-returning functions:** `stores.read_field().ok()?`
- **`()`-returning functions:** `let Ok(x) = stores.read_field() else { error!(...); return; }`
- **Other return types (String, bool, Vec, usize):** `let Ok(x) = stores.read_field() else { return default }`
- **Closures returning Option:** `.ok()?` inside `.and_then()`
- **`if let` chains with `store.lock()`:** Split into `let Ok(mut s) = ...` + `let Err(e) = s.update(...)`
- **Where entire body is inside lock scope:** `if let Ok(x) = ... && let Some(y) = ... { body }`

Signature changes in integrator.rs:
- `next_tick_number()`: `u32` → `Result<u32>`
- `has_tick_in_progress()`: `bool` → `Result<bool>`
- `recover_stuck_ticks()`: `u32` → `Result<u32>`

#### Phase 4: Daemon Infrastructure — COMPLETE

**Files:** `src/daemon/mod.rs`, `src/daemon/supervisor.rs`, `src/daemon/work_queue.rs`, `src/daemon/context.rs`

Eliminated ~45 production unwraps:

1. `recover_orphaned_records()` returns `usize` — used `let Ok(x) = ... else { return 0 }`
2. `graceful_shutdown()` returns `()` — used `let Ok(x) = ... else { error!(...); return }`
3. `store.lock().unwrap()` in `if let` chains — split into chained `&& let Ok(mut s) = ...`
4. Supervisor loop — used `let Ok(x) = ... else { continue }`

#### Phase 5: Periphery + Regression Prevention — COMPLETE

**Files:** `src/tui/run.rs`, `src/cli/dispatch.rs`, `src/lib.rs`, `src/ipc/codec.rs`, + all 81 test modules

1. `cli/dispatch.rs`: Replaced `resp.error.unwrap()` with `if let Some(err) = resp.error`
2. `tui/run.rs`: Replaced `client.as_mut().unwrap()` with `.expect("guarded by is_some")` — justified because the tokio `select!` branch guard ensures `client.is_some()`
3. Added `#![deny(clippy::unwrap_used)]` to `src/lib.rs` — future production `unwrap()` is now a compile error
4. Added `#[allow(clippy::unwrap_used)]` to all 81 `#[cfg(test)]` modules
5. `otto ci` green, all 1843 tests pass, zero production unwraps confirmed

## Alternatives Considered

### Alternative 1: `expect()` Instead of Helpers

- **Description:** Replace `.unwrap()` with `.expect("context message")` everywhere
- **Pros:** Minimal code change, better panic messages
- **Cons:** Still panics. Doesn't solve the cascade failure problem. Not idiomatic for an eyre codebase.
- **Why not chosen:** Doesn't actually fix the problem — just adds a message before crashing

### Alternative 2: Global Lock Helper Function (Not Method)

- **Description:** Free function `fn read_lock<T>(lock: &RwLock<T>) -> Result<RwLockReadGuard<T>>`
- **Pros:** No need for per-field methods
- **Cons:** Loses field-name context in error messages. Callers still write `read_lock(&stores.plans)?` which is barely shorter than `stores.plans.read().map_err(...)`. Doesn't compose with Mutex helpers.
- **Why not chosen:** Typed methods on `Stores` are more ergonomic and produce better error messages

### Alternative 3: `parking_lot` Locks (No Poisoning)

- **Description:** Replace `std::sync::RwLock`/`Mutex` with `parking_lot` equivalents, which never poison
- **Pros:** `.read()` and `.write()` return guards directly (no `Result`), so no error handling needed
- **Cons:** Major dependency change. Masks the real problem — if a thread panics while holding a lock, shared state may be corrupted. `parking_lot` just lets you access the corrupted state without warning.
- **Why not chosen:** Wrong layer to solve this. Poisoning is a feature that warns about corrupted state. We should handle the warning, not suppress it.

## Technical Considerations

### Dependencies

- **`paste`** crate — zero-dependency proc macro for identifier concatenation in `store_accessors!`. Well-established crate (850M+ downloads).

### Performance

Zero impact. The `.map_err()` call on a lock that succeeds is compiled away (the closure is never invoked). The `try_handler!` macro adds a single match, which is a no-op on the happy path.

### Testing Strategy

1. **`otto ci`** — full lint/check/test pipeline validates no regressions
2. **Existing test suite** — all handler tests, agent tests, and integration tests exercise the lock paths
3. **Lock poisoning test** — add one test that intentionally poisons a lock and verifies the helper returns `Err` instead of panicking:

```rust
#[test]
fn test_poisoned_lock_returns_error() {
    let stores = Stores::new();
    // Poison the plans lock by panicking while holding it
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = stores.plans.write().unwrap();
        panic!("intentional");
    }));
    // Now read_plans() should return Err, not panic
    assert!(stores.read_plans().is_err());
}
```

### Verification

After each phase:
```bash
otto ci          # Full pipeline: lint + check + test
cargo check      # Quick compile check between batches
```

After all phases:
```bash
# Verify zero production unwraps remain
rg '\.unwrap\(\)' src/ --type rust | grep -v '#\[cfg(test)\]' | grep -v 'mod tests'
```

(Manual review needed to confirm remaining hits are truly in test code.)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Missing an unwrap in production code | Medium | Low | Post-fix grep audit; add clippy lint `#![deny(clippy::unwrap_used)]` in lib.rs with `#[cfg_attr(test, allow(clippy::unwrap_used))]` on test modules |
| `paste` crate introduces build issues | Very Low | Low | Well-established crate; fallback is writing accessors manually |
| Handler error messages lose specificity | Low | Low | `try_handler!` maps to `RpcError::internal()` which preserves the eyre chain |
| Lock poisoning in practice is already fatal | High | None | True, but the fix also catches all non-lock unwraps (serde, HashMap, Option) which are real risks |

### Preventing Regression

After all phases complete, add to `src/lib.rs` (or `src/main.rs`):

```rust
#![deny(clippy::unwrap_used)]
```

Test modules already use `#[cfg(test)]` — clippy respects `#[allow(clippy::unwrap_used)]` on test modules. Add this to each test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests { ... }
```

This makes future production `unwrap()` a compile error.

### Edge Case: `unwrap()` Inside Closures

Some `unwrap()` calls are inside closures passed to `.map()`, `.filter()`, or `.for_each()` where `?` doesn't work. These need refactoring to:

- `.and_then()` chains for `Option`
- `.ok_or_else()` to convert `Option` → `Result` before the closure
- Extracting the closure body into a function that returns `Result`

### `unwrap_or_default()` and `unwrap_or()`

These are **not** targets. They have explicit fallbacks and are idiomatic. Only bare `.unwrap()` is the problem.

## Open Questions

None — approach validated by Phases 1–2 implementation.

## Implementation Notes

### Phases 1–2 (Handlers)

1. **`try_handler!` needed clippy suppression.** The immediately-invoked closure `(|| { ... })()` triggers `clippy::redundant_closure_call`. Fixed with a scoped `#[allow]` and double-brace `{{ }}` block syntax to let the attribute attach correctly.

2. **`find_latest_published_tick` returns `Option`, not `Result`.** Used `stores.read_ticks().ok()?` to convert the `Result` to `Option`, matching the function's return type.

3. **Handler-to-handler delegation.** `handle_integrator_seal` calls `handle_integrator_validate` which returns `DaemonResponse` directly. Inside the `try_handler!` closure (which expects `Result<DaemonResponse>`), this needed wrapping in `Ok(...)`.

4. **Automated transformation.** A Python script (`scripts/transform_handlers.py`) handled the bulk of the 194-unwrap transformation in `handlers.rs`. Manual fixups were needed for edge cases (items 2–3 above). The script was deleted after use.

### Phases 3–5 (Agent + Daemon + Periphery)

5. **Return type determines pattern.** Unlike handlers (which all return `DaemonResponse`), agent and daemon functions have diverse return types. The correct pattern depends on the enclosing function's signature — automated scripts must understand function context, not just pattern-match lines.

6. **`if let Ok(x)` vs `let Ok(x) = ... else`.** For functions where the entire body depends on a lock, `if let Ok(x) = ... && let Some(y) = ... { body }` is cleaner than `let Ok(x) = ... else { return }` because it avoids the ugly single-line else clause. Clippy enforces this via `collapsible_if`.

7. **Three integrator methods needed signature changes.** `next_tick_number()`, `has_tick_in_progress()`, and `recover_stuck_ticks()` changed from concrete types to `Result<T>`. This required updating test assertions to add `.unwrap()` — the only case where production signature changes rippled into test code.

8. **`#[allow(clippy::unwrap_used)]` placement matters.** On `#[cfg(test)]` items that aren't `mod` blocks (e.g., test-only `use` statements or `fn` items at module scope), the allow attribute triggers `clippy::useless_attribute` because the item itself doesn't use unwrap. Only place it on `mod tests { ... }` blocks.

## References

- [Rust `std::sync::RwLock` poisoning docs](https://doc.rust-lang.org/std/sync/struct.RwLock.html#poisoning)
- [eyre crate](https://docs.rs/eyre/)
- [paste crate](https://docs.rs/paste/)
- [clippy `unwrap_used` lint](https://rust-lang.github.io/rust-clippy/master/index.html#unwrap_used)
