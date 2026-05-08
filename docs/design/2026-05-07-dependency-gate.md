# Design Document: Dependency DAG Enforcement

**Author:** Scott A. Idler
**Date:** 2026-05-07
**Status:** Implemented
**Review Passes Completed:** 4/4
**Crates touched:** `loopr`, `domain`, `store`

## Summary

Work records carry a `dependencies: Vec<WorkId>` field populated by the decomposer, but the daemon ignores it entirely: all Works are spawned simultaneously at plan-creation time regardless of the dependency graph. This doc defines a reactive dep gate that (a) filters the initial dispatch so only dep-free Works start immediately, and (b) promotes newly unblocked Works after each sibling reaches `Done`, without polling and without an event bus.

## Problem Statement

### Background

The decomposer produces a typed dependency DAG: each `Work.dependencies` holds the `WorkId`s that must be `Done` before this Work may begin. `crates/decomposer/src/cycles.rs` detects cycles at decompose time. `crates/decomposer/src/decompose.rs:378` wires the resolved `WorkId`s into the Work record. The field exists; the enforcement does not.

### Problem

`crates/loopr/src/transport/handler.rs:182-184` spawns every Work immediately after decomposition:

```rust
for work in works {
    let task_ctx = Arc::clone(ctx);
    tasks.spawn(task_ctx.spawn_implementer_for_work(work));
}
```

`spawn_implementer_for_work` then advances `Pending -> Ready -> InProgress` with no dep check. The result: all Works in a multi-Work plan race to `InProgress` simultaneously, even those whose dep Works have not yet begun. The python-api E2E (referenced in `docs/deferred-roadmap.md` 1.1) showed three Works hitting `InProgress` concurrently despite a 3-node DAG where node 3 depended on nodes 1 and 2.

### Goals

- Only Works whose entire `dependencies` set is `Done` may advance from `Pending` to `Ready`.
- When a Work reaches `Done`, the daemon immediately checks sibling Works for newly unblocked candidates and spawns Implementers for them.
- When a dep reaches a truly terminal non-Done state (`Abandoned`, `Superseded`), its dependents are marked `Blocked` with a reason (preparing for 1.3's recovery loop). `Blocked` itself is not terminal - a Blocked dep may recover via 1.3, so dependents stay `Pending` until the dep either recovers to `Done` or reaches `Abandoned`/`Superseded`.
- Zero polling: no background ticker, no repeated store scans. Every promotion is triggered by a state-change event already happening.
- The fix lives exclusively in `crates/loopr`; `crates/domain` gets a small pure helper.

### Non-Goals

- A full Coordinator/Director agent (1.2). The dep gate here is the daemon's Reactor doing deterministic routing, not LLM orchestration.
- Parallel Implementer dispatch (3.3). This doc enforces serial-per-dep-chain; concurrency within independent dep chains is allowed but not explicitly managed here.
- The `blocked_reason` recovery loop (1.3). This doc writes `blocked_reason` but does not read it; 1.3 owns the retry path.
- Event bus (3.2). Reactive promotion here is achieved by a synchronous sibling sweep in the `Done` transition path - no `tokio::sync::broadcast` needed at this tier.

## Proposed Solution

### Overview

Two insertion points, zero polling:

1. **At dispatch time** (`handler.rs`): split newly decomposed Works into `unblocked` (all deps Done or no deps) and `held` (at least one dep not Done). Spawn Implementers only for `unblocked`. `held` Works remain `Pending` in the store; no task is spawned for them yet.

2. **After `Done`** (`context.rs`): after every `Integrated -> Done` transition, call `promote_unblocked_siblings`. This function lists all Pending sibling Works, checks each one's `dependencies` against the current store state, and calls `spawn_implementer_for_work` for any newly eligible Work.

3. **After terminal non-Done** (`context.rs`): if a Work reaches `Abandoned` or `Superseded` (truly terminal, not recoverable), call `block_dependent_siblings`. This marks any Pending Work that listed this Work as a dep as `Blocked` (override transition) with a `blocked_reason` string. `Blocked` deps do not trigger this path - a Blocked dep may still recover via 1.3 and eventually reach `Done`, so its dependents wait.

### Architecture

```
handler.rs (plan.create)
  decompose -> [works]
    partition: unblocked | held
    spawn implementer for each unblocked
    held Works stay Pending in store

context.rs (spawn_implementer_for_work -> Done path)
  Integrated -> Done (Reactor)
    promote_unblocked_siblings(plan_id, done_work_id, store, ctx)
      list Pending siblings
      for each: all_deps_done(work.dependencies, store) ?
        spawn_implementer_for_work(sibling)

context.rs (any terminal non-Done path)
  Work -> Abandoned | Superseded
    block_dependent_siblings(plan_id, terminal_work_id, store, ctx)
      list Pending siblings whose dependencies contains terminal_work_id
      for each: transition Pending -> Blocked (override, Reactor)
                set work.blocked_reason = "dep <id> reached <status>"

startup.rs (reconcile, after Work/Bundle sweeps)
  for each Active Plan:
    promote_unblocked_siblings(plan_id, None, store, ctx)
    block_dependent_siblings for any terminal non-Done Work if Pending siblings still reference it
```

### Data Model

`Work.blocked_reason` is added as an optional field deferred by scope memo D3 in `docs/design/2026-04-20-hierarchy.md`. This doc ships it:

```rust
#[serde(default)]
pub blocked_reason: Option<String>,
```

All other Work fields are unchanged. The `dependencies: Vec<WorkId>` field already exists.

Pure helper added to `crates/domain/src/work.rs`:

```rust
/// True when every dep id in `self.dependencies` appears in `siblings`
/// with status `Done`. Unknown dep ids (not in `siblings`) return false.
pub fn all_deps_done(&self, siblings: &[Work]) -> bool {
    self.dependencies.iter().all(|dep_id| {
        siblings.iter().any(|s| &s.id == dep_id && s.status == WorkStatus::Done)
    })
}

/// Returns the first dep id whose Work appears in `siblings` with a
/// truly terminal non-Done status (`Abandoned` or `Superseded`). These
/// are the only states from which a Work cannot recover; `Blocked` is
/// excluded because it may recover via 1.3's recovery loop.
pub fn any_dep_irrecoverable(&self, siblings: &[Work]) -> Option<&WorkId> {
    self.dependencies.iter().find(|dep_id| {
        siblings.iter().any(|s| {
            &s.id == *dep_id
                && matches!(s.status, WorkStatus::Abandoned | WorkStatus::Superseded)
        })
    })
}
```

### Implementation Plan

#### Phase 0: Prerequisites - WorksStore OCC + FSM edge
**Model:** sonnet

Two missing pieces must land before any other phase can be safe:

**WorksStore OCC.** `crates/store/src/works.rs:168` exposes `update(work: Work)` with no `expected_updated_at` parameter. The comment at line 179 explicitly notes this gap. Without OCC, concurrent `promote_unblocked_siblings` calls both read the same Pending Work, both call `spawn_implementer_for_work`, and both succeed - resulting in two Implementer tasks for the same Work and worktree corruption. Upgrade `WorksStore::update` to `update(work: Work, expected_updated_at: i64)` mirroring `BundlesStore::update` exactly. Update `WorkUpdateSink::update` trait signature and all impls (real + forwarding). Update `transition_and_persist_work` to pass `work.updated_at` (snapshot before mutation) as the OCC token. Add `WorkUpdateError::Stale` variant. `spawn_implementer_for_work` treats `Stale` as a benign race and returns without error.

**`Pending => Blocked` FSM transition.** `crates/domain/src/work.rs` lines 42-73 define the Work FSM. `Pending => Blocked` appears in neither `transitions` nor `overrides`. Phase 4's `block_dependent_siblings` cannot compile without it. Add `Pending => Blocked by (Reactor)` to the `transitions` block. Add tests in `crates/domain/tests/work.rs` asserting the new edge is accepted and that `Pending => Blocked by (Director)` is rejected.

- `otto ci` green.

#### Phase 1: `blocked_reason` field and pure helpers in `domain`
**Model:** sonnet

- Add `pub blocked_reason: Option<String>` to `Work` struct with `#[serde(default)]`.
- Add `all_deps_done(&self, siblings: &[Work]) -> bool` method on `Work`.
- Add `any_dep_irrecoverable<'a>(&self, siblings: &'a [Work]) -> Option<&'a WorkId>` method on `Work`. Covers `Abandoned` and `Superseded` only; `Blocked` is excluded.
- Add unit tests in `crates/domain/tests/work.rs` for both helpers covering: no deps (always true/None), all Done (true/None), mixed Done+Pending (false/None), unknown dep id (false/None), dep Abandoned (Some), dep Superseded (Some), dep Blocked (None - Blocked is recoverable).
- `otto ci` green.

#### Phase 2: Filter initial dispatch in `handler.rs`
**Model:** sonnet

- In `handle_plan_create` (handler.rs), after `create_many`, fetch the full sibling list (already in `works` vec from decomposer output) and partition into `unblocked` and `held` using `work.all_deps_done(&works)`.
- Spawn Implementers only for `unblocked`.
- Log at `info!` level: `dep_gate.dispatch: unblocked={} held={}`.
- `otto ci` green.

#### Phase 3: Post-Done sibling promotion in `context.rs`
**Model:** sonnet

- Add `async fn promote_unblocked_siblings(self: &Arc<Self>, plan_id: &PlanId, done_work_id: &WorkId)` to `DaemonContext`. It:
  1. Lists all Works for `plan_id`.
  2. Filters to `Pending` status only.
  3. For each Pending sibling, calls `sibling.all_deps_done(&all_siblings)`.
  4. Spawns `spawn_implementer_for_work(sibling)` for each newly eligible Work.
  5. Logs at `info!` with `promoted_count`.
- Call `promote_unblocked_siblings` after the `Integrated -> Done` transition at context.rs:727.
- Extend `startup.rs:reconcile` to call `promote_unblocked_siblings` for each Active Plan after the existing Work/Bundle sweeps. This closes the crash-recovery gap (a dep that went Done before a crash would leave its dependents Pending forever without this sweep).
- `otto ci` green.

#### Phase 4: Dep-terminal blocking in `context.rs`
**Model:** sonnet

- Add `async fn block_dependent_siblings(self: &Arc<Self>, plan_id: &PlanId, terminal_work_id: &WorkId, terminal_status: WorkStatus)` to `DaemonContext`. It:
  1. Lists all Pending siblings for `plan_id`.
  2. Filters to Works whose `dependencies` contains `terminal_work_id`.
  3. For each: sets `work.blocked_reason = Some(format!("dep {} reached {:?}", terminal_work_id, terminal_status))` then fires the `Pending -> Blocked` override transition.
  4. Logs at `warn!` with `blocked_count`.
- Call `block_dependent_siblings` wherever a Work transitions to `Abandoned` or `Superseded`. As of this doc, context.rs has no existing call sites that write `Abandoned` or `Superseded` directly (current failure paths write `Blocked`). Phase 4 adds the two new `block_dependent_siblings` hooks to the locations that WILL write these states: the integration-failure escalation path (currently writes `Blocked`; a future commit may escalate to `Abandoned` after retry budget exhaustion), and any explicit `Superseded` writer added by 1.3 or 2.2. Phase 4 also adds an `#[allow]`-free TODO marker at each of today's `Blocked` terminal paths to remind future contributors. Do NOT call on `Blocked` - Blocked is recoverable.
- `otto ci` green.

#### Phase 5: Integration test
**Model:** sonnet

- Add `crates/loopr/tests/dep_gate.rs` using the existing process-level test harness pattern from `tests/stage_7_wiring.rs` and `tests/stage_8_plan_to_tick.rs`:
  - `loopr init` on a temp target repo.
  - `loopr plan "..."` with an `ANTHROPIC_API_KEY` that routes to a mock server (or use the `wiremock` pattern already in `crates/llm/tests/`).
  - The mock decomposer response returns 3 Works: A (no deps), B (deps: [A]), C (deps: [B]).
  - The mock Implementer for A immediately produces a Bundle that the mock Reviewer accepts.
  - After A Done: assert B transitions to `Ready` / `InProgress`; assert C stays `Pending`.
  - After B Done: assert C transitions to `Ready` / `InProgress`.
  - Full plan reaches `Complete`.
  - State assertions via `loopr list works -C <target>` JSON output, parsed in test.
- `otto ci` green.

## Alternatives Considered

### Alternative 1: Check deps inside `spawn_implementer_for_work`, return early if unmet

- **Description:** Add a `deps_met` check at the top of `spawn_implementer_for_work`. If deps not met, return immediately. Re-examine later via... what mechanism?
- **Pros:** Single change point; no partition at dispatch.
- **Cons:** The spawned task exits silently, the Work stays Pending forever, and nothing ever re-examines it. Requires either polling (a background ticker scanning Pending Works) or the event bus (3.2) to re-trigger - both are heavier than a targeted sibling sweep at Done time.
- **Why not chosen:** Leaves Pending Works stranded with no re-trigger mechanism available at Tier 1.

### Alternative 2: Poll: background ticker scans Pending Works every N seconds

- **Description:** A `tokio::time::interval` loop that wakes every N seconds, lists all Pending Works, checks each dep, promotes ready ones.
- **Pros:** No changes to dispatch or Done path; no sibling sweep.
- **Cons:** Adds latency proportional to tick interval. Introduces background state and a configurable knob (what's N?). Wasted work when no plan is running.
- **Why not chosen:** v5 is explicitly reactive. Every promotion event we need is already happening (Done transition); hooking into it is simpler and lower latency.

### Alternative 3: Typed event bus (3.2) with Reactor subscribing to Done events

- **Description:** Ship 3.2 first; Reactor subscribes to `DaemonEvent::WorkDone` and promotes siblings.
- **Pros:** Clean separation; Reactor decoupled from the Done transition call site.
- **Cons:** 3.2 is Tier 3 (depends on 1.2 Director, which depends on 2.4 multi-turn LLM). Blocking dep gate on 3.2 means no multi-Work plans work until three tiers are complete.
- **Why not chosen:** The synchronous sibling sweep achieves the same result without the event bus; 3.2 can migrate the call site to a subscription when it ships.

## Technical Considerations

### Dependencies

- No new external crate dependencies.
- `store` crate change (Phase 0): `WorksStore::update` gains `expected_updated_at: i64`; `WorkUpdateSink` trait updated; `WorkUpdateError::Stale` added. Mirrors `BundlesStore` exactly.
- `domain` crate change (Phase 0 + Phase 1): `Pending => Blocked` FSM edge added; `blocked_reason` field added; two pure helpers added.

### Performance

- `promote_unblocked_siblings` calls `list_by_parent_id` once per Done transition. For a flat Plan -> Work structure, this is one store read per completed Work - acceptable.
- `block_dependent_siblings` is the same shape. Both are O(n) in siblings count.

### Security

None. These are internal state transitions on local TaskStore records.

### Testing Strategy

- **Unit tests (Phase 1):** `all_deps_done` and `any_dep_terminal_non_done` cover the no-dep, all-done, partial, unknown-dep, and terminal-non-done cases.
- **Seam tests (Phase 5):** the 3-node DAG integration test exercises the full reactive promotion chain without spinning up real LLM calls (Implementer mock returns Done immediately).
- **Regression:** existing tests (all Works have empty `dependencies`) continue to pass; the partition at dispatch produces all Works in `unblocked`.

### Rollout Plan

Single branch on `v5`. Five phases, each committed separately with `otto ci` green. No feature flag needed - the gate is a pure correctness fix and the old behavior (spawn all immediately) is already incorrect for any multi-Work plan with deps.

### Daemon Restart Recovery

The startup reconcile (`startup.rs:reconcile`) runs before the IPC listener binds. After a crash, the store may have:
- Works in `Done` whose dependents are still `Pending` (they were never promoted before the crash).
- Works in `Abandoned` / `Superseded` whose dependents are still `Pending` (they were never blocked before the crash).

Phase 3 adds a reconcile extension: after the existing Work and Bundle sweeps, for each `Active` Plan, call `promote_unblocked_siblings` (no `done_work_id` filter - sweep all Pending siblings) and `block_dependent_siblings` for any terminal non-Done Work that still has Pending dependents. This closes the crash-recovery gap without a separate design doc.

### Unknown Dep WorkIds

`all_deps_done` returns `false` for dep ids not found in `siblings` (unknown id). This is correct - an unknown dep is treated as unsatisfied. However, if the decomposer emits a WorkId that doesn't exist in the store (a bug), the dependent Work is permanently Pending with no recovery path. Defense: at dispatch time (Phase 2), after partitioning, emit a `warn!` for any Work in `held` whose `dependencies` contains an id not present in the `works` vec. This surfaces the bug immediately rather than leaving a silent Pending Work.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `promote_unblocked_siblings` called concurrently for two sibling Done transitions, both finding the same Work eligible | Medium | High | Phase 0 adds OCC to `WorksStore::update`. `transition_and_persist_work` passes the pre-mutation `updated_at` as the OCC token. The second concurrent writer receives `WorkUpdateError::Stale` and exits without spawning a second Implementer. Without Phase 0's OCC, this race results in two concurrent Implementers on the same Work and worktree corruption. |
| A dep chain longer than store read budget causes slow promotion cascade | Low | Low | Each promotion is one store read; a 10-node chain causes 10 sequential reads, each O(n siblings) - negligible for realistic plan sizes |
| `blocked_reason` field causes `deny_unknown_fields` deserialization failures on existing JSONL written before this change | Low | High | `#[serde(default)]` on the field makes it optional for deserialization; existing records without the field deserialize with `None` |
| `block_dependent_siblings` is not called at every terminal non-Done transition path | Medium | Medium | Phase 4 audits all terminal-transition call sites in `context.rs` and adds the call; the integration test in Phase 5 exercises the Blocked-dep path explicitly |

## Open Questions

- [x] **Concurrent Done promotions - resolved by Phase 0.** WorksStore has no OCC today (`works.rs:179` explicitly notes this). Without it, two concurrent Done transitions both finding the same Pending Work eligible would spawn two Implementers and corrupt the worktree. Phase 0 adds OCC to `WorksStore::update`; `transition_and_persist_work` passes the pre-mutation `updated_at` as the version token. The second concurrent writer receives `WorkUpdateError::Stale` and exits cleanly. This is not a post-hoc guard - it is a prerequisite for correctness.
- [ ] **Cascade blocking is explicitly excluded.** `block_dependent_siblings` only blocks Works whose `dependencies` directly list the terminal Work. It does not cascade: if Work C is `Abandoned`, Work B becomes `Blocked`, but Work A (which depends on B) stays `Pending` indefinitely - because B is `Blocked`, not terminal, so `any_dep_irrecoverable` returns `None` for A. The operational consequence: A hangs in `Pending` until manual intervention on B (via 1.3's recovery or operator override). This is intentional at Tier 1. A smarter cascade would require knowing whether a Blocked dep can ever recover, which requires 1.3's recovery budget logic. Document this in the operator runbook when 1.3 ships.
- [ ] **Decomposer cycle defense:** the decomposer detects cycles before persisting. If a cycle somehow escapes into the store, both Works wait for each other and both stay Pending forever. Defense-in-depth: add a `warn!` in `promote_unblocked_siblings` when a Pending Work's deps are all terminal but none are Done (unreachable under normal conditions). Sufficient for Tier 1; a proper cycle scan is Tier 5.

## References

- [`docs/deferred-roadmap.md` §1.1](deferred-roadmap.md) - this doc's stub with source-material pointers
- `crates/loopr/src/transport/handler.rs:182-184` - the unconstrained dispatch loop
- `crates/loopr/src/daemon/context.rs:278-306` - `spawn_implementer_for_work` entry point
- `crates/loopr/src/daemon/context.rs:700-753` - `Integrated -> Done` path and sibling completion check
- `crates/domain/src/work.rs:115` - `dependencies: Vec<WorkId>` field
- `~/repos/scottidler/loopr/src/daemon/work_queue.rs:27-100` - v3 `next_assignable_work` (polling model, reference only)
- `docs/design/2026-04-20-hierarchy.md` D3 - `blocked_reason` deferral (resolved by this doc)
