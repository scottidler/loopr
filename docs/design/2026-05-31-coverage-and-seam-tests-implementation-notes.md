# Implementation Notes: coverage task + loopr seam tests

Companion to `docs/design/2026-05-31-coverage-and-seam-tests.md`. Append-only.

## Phase 1: handler.rs dispatch seam gaps

### Design decisions
- Tested `map_store_error` directly as a pure function (`map_store_error_maps_each_variant`) covering all 7 `StoreError` arms, rather than trying to provoke each store failure through the dispatch path — `crates/loopr/src/transport/handler/tests.rs`. Provoking `Stale`/`DuplicateTick`/`Corruption` through real store I/O would be far more machinery for the same assertion.
- `record_get` happy path asserts on the deserialized `ipc::RecordResult::Plan` rather than raw JSON, matching the `record_list` test's style.

### Deviations
- None. The plan named `handle_record_get` and `map_store_error`; both are covered.

### Tradeoffs
- Covered the `RecordNotFound -> NotFound` arm twice (once via the live `record_get_nonexistent_id_yields_not_found` dispatch path, once via the direct `map_store_error` test). Kept both: the dispatch test proves `store_err_response` wiring, the direct test proves the mapping in isolation. Cheap.

### Open questions
- None.

## Phase 2: startup.rs cold-boot re-drive seam

### Design decisions
- Placed the test in-crate (`crates/loopr/src/daemon/startup/tests.rs`) because `sweep_bundles` is private and `daemon::startup` is not a `pub mod` (so `tests/` integration files cannot reach `reconcile`/`sweep_bundles`). Added an in-crate `build_ctx` helper mirroring `tests/director_stuck_states.rs::build_test_context`.
- Set `ctx.shutting_down = true` BEFORE calling `sweep_bundles` so the spawned reviewer/integrator task bodies short-circuit at their documented defensive entry guard. `sweep_bundles` itself has no `shutting_down` check, so it still runs the full routing loop and records its counters. This gives deterministic counter assertions with zero background work — deliberately avoiding the reviewer/director-spawn + teardown machinery that produced the 7-hour CI hang on 2026-05-24.
- Seeded terminal bundle statuses (`Merged`, `Rejected`) via a direct `b.status = ...` field set, since terminal statuses are not reachable from `Proposed` via a single FSM edge. Matches the existing `work.status = WorkStatus::Done` shortcut in the sibling worktree tests.

### Deviations
- The plan also named `sweep_dep_promotions` as a Phase 2 target. Deferred: its promotion path spawns implementers and requires a Work whose deps just reached Done, which reintroduces the background-task/teardown complexity Phase 2 was scoped to avoid; and `tests/dep_gate.rs` already exercises dep-gate promotion. `sweep_bundles` was the higher-value, lower-risk half. Noted as an open question rather than forced.

### Tradeoffs
- Could have tested only terminal bundles (zero spawns, zero risk) but that would miss the reviewer/integrator requeue arms, which are the more important recovery behavior. The `shutting_down` guard lets all three arms be covered safely, so all four statuses are seeded.

### Open questions
- Is a `sweep_dep_promotions` cold-boot test worth adding (promote-on-restart of a Work whose dep went Done before the crash), given `tests/dep_gate.rs` already covers the in-flight dep gate? Lower priority.

## Phase 3: confirm + close

### Design decisions
- Ran `otto ci` (green) and `otto cov` as the gates. Recorded the per-file delta in the design doc Testing Strategy: `handler.rs` 79% -> 94%, `startup.rs` 71% -> 82%, workspace 92.0% -> 92.1%.
- Ran `cargo llvm-cov clean` after measuring to release the ~13G instrumented target tree (regenerable).

### Deviations
- The cov run was briefly deferred mid-session after an unrelated disk-full event (the `llvm-cov-target` doubling on a 97%-full disk corrupted the harness config). Re-ran once ~380G was reclaimed. No change to the tests or plan.

### Tradeoffs
- None.

### Open questions
- None.
