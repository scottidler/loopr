# Design Document: Coordinator Planning Stuck and Supervisor Restart Counter Fix

**Author:** Scott A. Idler
**Date:** 2026-04-14
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Two coordinator bugs were confirmed during a `rust-version` E2E run. Bug 1: the coordinator
enters `Planning` and loops indefinitely because `decompose_hierarchy` never sets `plan.tier =
Tier::Brief`, so `check_fsm_transition` always evaluates the Full path (requiring Specs) even when
the plan was decomposed as Brief (Works directly under the Plan). Bug 2: the supervisor restart
counter resets on every `Running` entry, making the 5-attempt ceiling effectively infinite. Both
fixes are minimal, independent, and target-isolated.

## Problem Statement

### Background

The coordinator FSM has five states: `Decomposing -> Planning -> Executing -> GoalComplete`. The
`check_fsm_transition` function evaluates Store state on each iteration to determine if a transition
is warranted. For `Planning -> Executing`, the guard branches on `plan.tier`:

- `Tier::Brief`: transition when at least one Work is parented directly to the Plan
- `Tier::Full`: transition when at least one Spec is parented to the Plan

Brief mode exists because trivial goals ("add a --version flag") produce no Specs or Phases - the
LLM decomposes the Plan directly into Works.

The supervisor (`loopr-v4/src/daemon/supervisor.rs`) wraps the coordinator with
exponential-backoff restart logic and a 5-attempt ceiling controlled by `restart_count`.

### Problem

**Bug 1: `plan.tier` is never set to `Tier::Brief`**

In `loopr/src/daemon/handlers/doc.rs` (line 232), `classify_brief` is called and correctly returns
`true` for the `rust-version` goal. The `brief=true` value is passed into `decompose_hierarchy`.
Inside `decompose_hierarchy`, Brief mode is properly respected for child creation (Works are
created directly under the Plan instead of going through Specs/Phases). However, the Plan itself
is constructed inside `records_to_hierarchy` (line 847) as:

```rust
let mut plan = Plan::new(plan_title.to_string(), plan_ac);
plan.id = plan_id.to_string();
plan.force_status(PlanStatus::Active);
// plan.tier is never set - defaults to Tier::Full
```

`Plan::new()` produces `tier: Tier::default()` = `Tier::Full` (the `#[default]` variant in the
enum). The `brief` boolean is not in scope inside `records_to_hierarchy` - it controls which
children are created in `decompose_hierarchy` but never reaches the Plan construction step.

The Plan is persisted to JSONL with `tier=full`. When the coordinator enters `Planning`,
`check_fsm_transition` reads `plan.tier == Tier::Full`, takes the Full path, checks for Specs
(there are none - this was a Brief decomposition), and returns `None`. The coordinator never
advances to `Executing`. The Lifeguard detects 5 consecutive identical `done` actions from the
LLM and kills the session with `NeedsHelp`. The supervisor restarts. The cycle repeats.

**Bug 2: Supervisor restart counter resets on every `Running` entry**

In `loopr-v4/src/daemon/supervisor.rs` (line 86):

```rust
if status == AgentStatus::Running && restart_count > 0 {
    info!("Coordinator reached Running, resetting supervisor restart counter");
    restart_count = 0;
    continue;
}
```

This fires on every restart that reaches `Running` - which happens within milliseconds of spawn,
before the coordinator has done any useful work. Observed pattern from the E2E session log:

1. Coordinator fails - `restart_count` increments to 1
2. Supervisor restarts coordinator; coordinator reaches `Running`
3. `restart_count > 0` is true - counter resets to 0
4. Coordinator fails again - `restart_count` increments to 1 again
5. All three observed restarts show "attempt 1/5" in logs

The 5-attempt ceiling is never reached. The supervisor is an infinite restart loop.

### Goals

- Fix `plan.tier` so Brief plans are persisted with `Tier::Brief`, enabling
  `check_fsm_transition` to correctly route to the Brief path
- Fix the supervisor restart counter so the 5-attempt ceiling is effective against crash loops
- Keep both fixes minimal and targeted - no new FSM states, no new domain fields

### Non-Goals

- Changing `classify_brief` logic or the Brief/Full decomposition branching in `decompose_hierarchy`
- Modifying the Brief mode context hierarchy (Doc 5)
- Adding new `CoordinatorFsmState` variants
- Addressing any other E2E failures (integration, reviewing, bundling)
- Porting the v4 engine-driven decomposition path into `loopr` (separate effort per Doc 7)

### Note: v4 Already Has Bug 1 Fixed

`loopr-v4/src/daemon/handlers/doc.rs` (lines 242-245) already sets `plan.tier = Tier::Brief`
correctly in the engine-driven entry path:

```rust
let brief = classify_brief(stores, &markdown).await;
if brief {
    plan.tier = crate::domain::plan::Tier::Brief;
}
```

Bug 1 exists only in the `loopr` (v3) codebase that uses the background `decompose_hierarchy` task.
Phase 1 of this fix targets `loopr` exclusively. No changes to `loopr-v4` are needed for Bug 1.

## Proposed Solution

### Bug 1 Fix: Set `plan.tier` in `decompose_hierarchy` before persistence

The minimal fix is to mutate `hierarchy.plan.tier` after `records_to_hierarchy` returns but
before `persist_hierarchy` writes to JSONL. The `brief` variable is already in scope at both
call sites within `decompose_hierarchy`.

`records_to_hierarchy` is called in two places inside `decompose_hierarchy`:
- Line 681: partial failure early return (Full mode only, inside `else` branch)
- Line 693: normal completion path (both Brief and Full modes)

In Brief mode, only the line 693 path is reached. In both cases, the fix is the same:

```rust
// Current (line 693):
let hierarchy = records_to_hierarchy(&plan_id, &plan_title, plan_markdown, plan_ac, &all_records)?;
Ok((hierarchy, None))

// Fixed:
let mut hierarchy = records_to_hierarchy(&plan_id, &plan_title, plan_markdown, plan_ac, &all_records)?;
if brief {
    hierarchy.plan.tier = crate::domain::plan::Tier::Brief;
}
Ok((hierarchy, None))
```

Same pattern applies to line 681 (partial failure path):

```rust
let mut hierarchy = records_to_hierarchy(&plan_id, &plan_title, plan_markdown, plan_ac, &all_records)?;
if brief {
    hierarchy.plan.tier = crate::domain::plan::Tier::Brief;
}
return Ok((hierarchy, Some(err)));
```

`DecomposedHierarchy.plan` is a public `Plan` struct, so mutation is direct. The tier is set
in memory before `persist_hierarchy` writes the Plan to JSONL, so the on-disk record is correct
from the first write. No second update pass needed.

This approach requires exactly 2 changes in `decomposer.rs` production code, zero changes to
`records_to_hierarchy`'s signature, and zero test updates.

### Bug 2 Fix: Time-based heartbeat for restart counter reset

Replace the "reset on any `Running` entry" logic with "reset only if coordinator has been Running
continuously for at least 60 seconds." This is the systemd-style pattern: crashing quickly after
start counts as a bad restart; surviving for a meaningful duration before failing is treated as a
healthy run and the counter resets.

Add a `running_since: Option<tokio::time::Instant>` variable to the supervisor loop. Using
`tokio::time::Instant` (not `std::time::Instant`) ensures that `tokio::time::advance()` in
tests controls elapsed time correctly. When `Running` is observed for the first time in a
session, latch the instant. When a terminal status is observed, clear it.

```rust
const HEALTHY_UPTIME_SECS: u64 = 60;

let mut restart_count = 0u32;
let mut running_since: Option<tokio::time::Instant> = None;
```

Replace the current `Running` block (lines 86-90):

```rust
// Current:
if status == AgentStatus::Running && restart_count > 0 {
    info!("Coordinator reached Running, resetting supervisor restart counter");
    restart_count = 0;
    continue;
}

/// Fixed (latch only on first Running; coordinators cycle Running <-> WaitingForLlm per LLM call):
if status == AgentStatus::Running {
    let _ = running_since.get_or_insert_with(tokio::time::Instant::now);
    continue;
}

// Clear state on non-Failed terminal statuses (Completed, Cancelled) to prevent leakage
// into the next session:
if status.is_terminal() && status != AgentStatus::Failed {
    running_since = None;
    continue;
}
```

Replace the current `Failed` guard (before the `restart_count += 1` at line 115):

```rust
// Add before restart_count += 1:
let made_progress = running_since
    .take()
    .map(|t| t.elapsed().as_secs() >= HEALTHY_UPTIME_SECS)
    .unwrap_or(false);
if made_progress {
    info!(
        "Coordinator ran for >{}s before failing, resetting restart counter",
        HEALTHY_UPTIME_SECS
    );
    restart_count = 0;
}
```

`running_since.take()` clears the value atomically - no separate `running_since = None` needed.
`HEALTHY_UPTIME_SECS` is a module-level const, not a magic number.

The existing test `test_supervisor_resets_counter_on_running` encodes the old (incorrect)
behavior and must be updated: a bare `Running` event must NOT reset the counter. A new test
covers the time-based path using `tokio::time::advance` or a mock instant.

### Implementation Plan

#### Phase 1: Fix `plan.tier` in `decompose_hierarchy`
**Model:** sonnet

1. In `loopr/src/decomposer.rs`, locate the two `records_to_hierarchy` call sites at lines
   ~681 and ~693
2. Change both to `let mut hierarchy = records_to_hierarchy(...)?`
3. Add `if brief { hierarchy.plan.tier = crate::domain::plan::Tier::Brief; }` after each
4. Run `otto ci` - verify clean

#### Phase 2: Fix supervisor restart counter
**Model:** sonnet

1. Add `const HEALTHY_UPTIME_SECS: u64 = 60;` at module level in
   `loopr-v4/src/daemon/supervisor.rs`
2. Add `let mut running_since: Option<std::time::Instant> = None;` inside `run_supervisor`
3. Replace the `Running && restart_count > 0` block with `Running` -> set `running_since`
4. Add the `running_since.take()` progress check before `restart_count += 1`
5. Update `test_supervisor_resets_counter_on_running` to assert counter is NOT reset on bare
   `Running` event
6. Add `test_supervisor_resets_counter_after_healthy_uptime`: send `Running`, advance time by
   >60s, send `Failed`, assert counter was reset
7. Run `otto ci` - verify clean

#### Phase 3: E2E verification
**Model:** sonnet

1. Run `/e2e rust-version` against `loopr` with Phase 1 fix applied
2. Confirm coordinator advances `Planning -> Executing` in the session log
3. Confirm Works transition from `Pending` to `Ready` (via `promote_works` in reconcile)
4. Confirm goal reaches `GoalComplete`

## Alternatives Considered

### Alternative 1: Add `brief` parameter to `records_to_hierarchy`

- **Description:** Thread `brief: bool` into `records_to_hierarchy` and set `plan.tier` at
  construction time inside the function.
- **Pros:** Tier is set at the point of construction rather than mutated after. Slightly cleaner
  in terms of data flow.
- **Cons:** `records_to_hierarchy` has 9 test call sites plus 2 production call sites. All 11
  need a new `brief` argument. Most tests use `false` (they test Full mode structure). This is
  significant churn for a one-line fix.
- **Why not chosen:** 11 call site changes vs 2. The mutation approach is equally correct since
  the mutation happens before persistence.

### Alternative 2: Set tier in `doc.rs` background task after `persist_hierarchy`

- **Description:** After `persist_hierarchy` returns `Ok(())`, fetch the Plan from stores and
  call `store.update()` to set `plan.tier = Tier::Brief`.
- **Pros:** No changes to `decomposer.rs`.
- **Cons:** Two-phase write - Plan lands in JSONL as `Full`, then gets updated to `Brief`. If
  the process crashes between the two writes, the Plan on disk has wrong tier. Violates
  JSONL-as-truth: the first write should be correct, not patched after.
- **Why not chosen:** Two-phase writes create correctness windows. Fix the tier before it's
  persisted, not after.

### Alternative 3: Reset supervisor counter on `GoalComplete`

- **Description:** Replace the `Running` reset with a `GoalComplete` reset - only treat a
  fully-completed goal as "healthy."
- **Pros:** Simple. Semantically tight: only a fully-completed goal is truly healthy.
- **Cons:** Any plan that encounters more than 5 transient failures across its lifetime (network
  timeouts, transient OS faults) will be permanently abandoned even if each individual run made
  substantial progress. Long-running plans are systematically vulnerable.
- **Why not chosen:** Conflates infrastructure supervision with application business logic.
  The Architect's finding: a coordinator that runs for 3 hours and crashes once should not count
  against the ceiling.

### Alternative 4: Reset counter on FSM state advancement

- **Description:** Reset counter when coordinator advances FSM state (e.g., `Decomposing` to
  `Planning`).
- **Pros:** Progress-based; domain-aware.
- **Cons:** Couples the supervisor to coordinator FSM internals. The supervisor is infrastructure;
  it should not know about FSM states. Time-based uptime is simpler, decoupled, and matches
  proven supervisor patterns (systemd, runit, s6).
- **Why not chosen:** Unnecessary coupling. 60s of uptime is a sufficient and clean proxy.

## Technical Considerations

### Dependencies

- **Internal:** `Tier` enum in `loopr/src/domain/plan.rs` (already imported in `decomposer.rs`
  via `use crate::domain::plan::*`), `std::time::Instant` (stdlib)
- **External:** None

### Performance

No measurable impact. `Instant::now()` on `Running` events costs one syscall per coordinator
session start.

### Security

No security implications. Both fixes are internal logic changes.

### Testing Strategy

- Bug 1: The `rust-version` E2E run is the regression test. A unit test in `decomposer.rs`'s
  test module can assert that calling `decompose_hierarchy` with `brief=true` (mocked LLM)
  returns a `DecomposedHierarchy` with `plan.tier == Tier::Brief`.
- Bug 2: `test_supervisor_resets_counter_on_running` must be updated (bare `Running` no longer
  resets counter). New test `test_supervisor_resets_counter_after_healthy_uptime` uses
  `tokio::time::pause()` and `tokio::time::advance()` to simulate elapsed time without sleeping.

### Rollout Plan

- Phase 1 targets `loopr` (the E2E-running main repo at `~/repos/scottidler/loopr`)
- Phase 2 targets `loopr-v4` (the development branch at `~/repos/scottidler/loopr-v4`)
- Both require `otto ci` green before merge
- Phases 1 and 2 are independent and can be committed in either order
- Phase 3 E2E run verifies Bug 1 end-to-end against the running binary

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `Tier` import not available at the mutation site | Low | Low | `cargo check` fails immediately; `use crate::domain::plan::Tier` resolves it |
| Partial failure path (line 681) is inside `else` and `brief` is always `false` there | Low | Medium | Verified: Brief mode returns on line 635, never enters the `else` branch; but the fix is defensive and correct regardless |
| HEALTHY_UPTIME_SECS=60 too short for slow startup environments | Low | Low | Constant is visible and easy to tune; 60s is conservative for a coordinator that should reach `Planning` within 30s |
| Existing supervisor test encoding old behavior causes CI failure | High | Low | Expected - the test must be updated to the correct new behavior as part of Phase 2 |

## Open Questions

- [ ] Should `HEALTHY_UPTIME_SECS` be promoted to `SupervisorConfig` as a configurable field,
  or is a module-level constant sufficient?

## References

- `loopr/src/daemon/handlers/doc.rs` - background decomposition task (line 232: `classify_brief`)
- `loopr/src/decomposer.rs` - `decompose_hierarchy` (line 620) and `records_to_hierarchy` (line 836)
- `loopr/src/agents/coordinator.rs` - `check_fsm_transition` Planning branch (line 1058)
- `loopr/src/domain/plan.rs` - `Tier` enum (`Tier::Full` is `#[default]`, `Tier::Brief` is explicit)
- `loopr-v4/src/daemon/supervisor.rs` - supervisor restart loop (line 86: counter reset bug)
- `docs/design/2026-04-05-brief-mode-context-hierarchy.md` - Brief mode architecture (Doc 5)
- `docs/design/2026-04-05-coordinator-runaway-fix.md` - Lifeguard and runaway coordinator fixes
