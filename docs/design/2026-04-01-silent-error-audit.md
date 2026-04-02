# Design Document: Silent Error Elimination Audit

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Comprehensive audit found 22 places in the agent pipeline where errors from state transitions and IPC calls are logged but not propagated, causing state machine inconsistencies that stall the orchestration pipeline. This document catalogs every instance and prescribes the fix for each.

## Problem Statement

### Background

The agent pipeline (Implementer -> Reviewer -> Integrator -> Coordinator) relies on state transitions at each hand-off point. When a transition fails, the agent logs a warning and continues execution as if it succeeded. This creates fractured states (e.g., Bundle=Merged but Work=InReview) that no agent can recover from.

### Problem

Every E2E run eventually hits one of these silent failures, causing the pipeline to stall. Fixes to individual instances (v0.1.47-v0.1.51) revealed the next silent failure underneath. The root cause is systemic: the codebase has a pervasive pattern of `if resp.is_error() { warn(...); }` without returning or propagating the error.

### Goals

- Eliminate all silent error swallowing in state transition code
- Every transition failure must either be propagated as an error OR trigger a concrete recovery action
- The Coordinator must be able to detect and recover from any inconsistent state

### Non-Goals

- Changing the FSM rules themselves
- Adding retry logic for transient failures (that's a separate concern)
- Making event sends (`let _ = event_tx.send(...)`) into hard errors (events are advisory)

## Audit Results

### Tier 1: Critical - State transitions that fail but execution continues

These directly cause pipeline stalls observed in E2E.

| # | File | Lines | What Fails | Consequence |
|---|------|-------|-----------|-------------|
| 1 | integrator.rs | ~678 | Bundle->Merged | Bundle stays Triaged, work marked Integrated |
| 2 | integrator.rs | ~714 | Work->Integrated | Work stays InReview, tick published |
| 3 | coordinator.rs | ~978 | Work->Done | Work stuck in Integrated forever |
| 4 | integrator.rs | ~738 | Tick->Failed (validation) | Tick stays Validating, bundles rejected anyway |
| 5 | integrator.rs | ~558 | Bundle->Rejected (merge fail) | Bundle stays Integrating, work not reset |
| 6 | integrator.rs | ~752 | Bundle->Rejected (validation fail) | Bundle stays Integrating, work not reset |

**Fix pattern:** Context-dependent. Pre-merge transitions (Bundle->Merged, Tick->Failed) can safely `return Err(...)` to abort. Post-merge transitions (Work->Integrated, Work->Done) happen AFTER code is already in main - returning Err would strand the tick and cause the recovery logic to reject already-merged bundles, decoupling git from the database. Post-merge failures must collect errors, publish the tick anyway, and emit a Learning/alert so the Coordinator can reconcile.

### Tier 2: High - Stale/recovery transitions that fail silently

These cause stuck states during recovery and stale bundle handling.

| # | File | Lines | What Fails | Consequence |
|---|------|-------|-----------|-------------|
| 7 | integrator.rs | ~164 | Tick->Failed (recovery) | Tick stuck Open, recovery loops |
| 8 | integrator.rs | ~181 | Tick->Validating (recovery) | Tick stuck Sealing |
| 9 | integrator.rs | ~199 | Tick->Failed (recovery) | Recovery fails, Learning skipped |
| 10 | integrator.rs | ~305 | Bundle->Rejected (stale) | Stale bundle stays, work not reset |
| 11 | integrator.rs | ~328 | Bundle->Rejected (replan) | Replan event lost, work not reset |
| 12 | integrator.rs | ~379 | Bundle->Rejected (auto-replay fail) | Rejection via `let _ =`, lost |
| 13 | integrator.rs | ~387 | Bundle.Update (auto-replay) | Bundle merged with stale base |

**Fix pattern:** Return `Err(...)` from recovery operations. The integrator cycle will retry on the next iteration.

### Tier 3: Medium - Reviewer transitions and metadata

These cause state/metadata mismatches but are less likely to stall the pipeline.

| # | File | Lines | What Fails | Consequence |
|---|------|-------|-----------|-------------|
| 14 | reviewer.rs | ~212 | Bundle->Reviewed | Approval mismatch (race condition) |
| 15 | reviewer.rs | ~242 | Bundle->Rejected | Learning says rejected but bundle unchanged |
| 16 | reviewer.rs | ~273 | Bundle->Rejected | Same as 15 |
| 17 | reviewer.rs | ~235,266 | Bundle.Update (verification) | Rejection reason metadata lost |

**Fix pattern for 14-16:** Return the error from the reviewer. The session ends with an error, triggering proper cleanup.
**Fix pattern for 17:** These are metadata updates, not transitions. Log and continue is acceptable, but should log at error level not via `let _ =`.

### Tier 4: Low - Fire-and-forget events and learnings

These are acceptable losses but should log failures.

| # | File | Lines | What Fails | Consequence |
|---|------|-------|-----------|-------------|
| 18 | integrator.rs | ~221 | Learning.Create | Recovery learning lost |
| 19 | coordinator.rs | ~1721 | Learning.Create | Exhaustion learning lost |
| 20 | bundle.rs, work.rs | handlers | Event send | UI events lost |

**Fix pattern:** Change `let _ =` to log a warning on failure. These are not pipeline-critical.

## Proposed Solution

### Phase 1: Integrator transitions (Tier 1 items 1-2, 4-6 + Tier 2)

The integrator has the most critical silent failures. The fix depends on whether the failure is pre-merge or post-merge.

**CRITICAL: Git operations are non-transactional with the database.** Once branches are merged into main, the integrator is committed. Returning `Err(...)` after a successful merge would strand the tick, and the recovery logic would reject already-merged bundles, decoupling git history from the database.

#### Pre-merge transitions (items 4-6, 7-13): `return Err(...)`

These happen before code reaches main. Aborting is safe - the tick fails, bundles get rejected, and the next cycle retries cleanly.

**Before:**
```rust
let resp = self.ctx.bridge.request("bundle.transition", ...);
if resp.is_error() {
    self.ctx.warn(&format!("failed to transition bundle {}: {:?}", id, resp.error));
}
```

**After:**
```rust
let resp = self.ctx.bridge.request("bundle.transition", ...);
if resp.is_error() {
    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
    return Err(eyre!("failed to transition bundle {}: {}", id, msg));
}
```

#### Post-merge transitions (items 1-2): collect errors, continue, emit Learning

These happen after code is already in main (the C1 block that transitions Work->Integrated). The integrator MUST finish the cycle and publish the tick. Failed work transitions are collected and emitted as Learnings so the Coordinator can reconcile.

**Before:**
```rust
let resp = self.ctx.bridge.request("work.transition",
    json!({"id": wi_id, "target_status": "Integrated", "role": "integrator"})
);
if resp.is_error() {
    self.ctx.warn(&format!("failed to transition WI {} to Integrated: {:?}", wi_id, resp.error));
}
```

**After:**
```rust
let resp = self.ctx.bridge.request("work.transition",
    json!({"id": wi_id, "target_status": "Integrated", "role": "integrator"})
);
if resp.is_error() {
    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
    self.ctx.error(&format!("CRITICAL: work {} stuck after merge - failed to transition to Integrated: {}", wi_id, msg));
    let _ = self.ctx.bridge.request("learning.create", serde_json::json!({
        "content": format!("Work {} has merged bundle but failed to transition to Integrated: {}. Coordinator must reconcile.", wi_id, msg),
        "scope": format!("work/{}", wi_id),
        "role": "integrator"
    }));
    // DO NOT return - tick must still be published since code is in main
}
```

The same pattern applies to Bundle->Merged (item 1): if the bundle transition to Merged fails after the git merge succeeded, collect the error and emit a Learning rather than aborting.

Files changed: `src/agents/integrator.rs`

### Phase 2: Coordinator Work->Done (Tier 1 item 3)

**Before:**
```rust
if resp.is_error() {
    log.warn(&format!("failed to transition WI {} Integrated->Done: {:?}", wi_id, resp.error));
}
```

**After:**
```rust
if resp.is_error() {
    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
    log.error(&format!("failed to transition WI {} Integrated->Done: {}", wi_id, msg));
    return;  // skip this work item, retry next cycle
}
```

Files changed: `src/agents/coordinator.rs`

### Phase 3: Reviewer transitions (Tier 3 items 14-17)

For the reviewer, transition failures during approval/rejection should end the session with an error so the work item gets properly recycled.

**Before:**
```rust
if resp.is_error() {
    self.ctx.warn(&format!("bundle {} already advanced: {:?}", self.bundle_id, resp.error));
}
```

**After:**
```rust
if resp.is_error() {
    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
    return Err(eyre!("failed to transition bundle {}: {}", self.bundle_id, msg));
}
```

Files changed: `src/agents/reviewer.rs`

### Phase 4: Fire-and-forget cleanup (Tier 4)

Change `let _ = bridge.request("learning.create", ...)` to log warnings on failure.

Files changed: `src/agents/integrator.rs`, `src/agents/coordinator.rs`, `src/agents/reviewer.rs`

## Alternatives Considered

### Alternative 1: Add retry loops around failed transitions

- **Pros:** Handles transient failures
- **Cons:** Adds complexity, hides bugs, could loop forever
- **Why not chosen:** The integrator/coordinator already run in cycles. Returning an error and retrying on the next cycle IS the retry mechanism.

### Alternative 2: Add a "state reconciliation" sweep

- **Pros:** Catches all inconsistencies regardless of cause
- **Cons:** Complex, papering over bugs instead of fixing them
- **Why not chosen:** Fix the source. Reconciliation is a safety net, not a primary strategy.

## Testing Strategy

- Existing tests verify the happy path. Add tests for each transition failure path:
  - Mock bridge returns error for bundle.transition -> verify integrator returns Err
  - Mock bridge returns error for work.transition -> verify coordinator skips and retries
  - Mock bridge returns error for bundle.transition in reviewer -> verify session ends with error
- Run lua-todo E2E after each phase to verify pipeline progression

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Returning Err from integrator causes tick to fail | Med | Med | The integrator cycle already handles Err by marking the tick as Failed and rejecting bundles. This is correct behavior. |
| Reviewer Err causes retry loop | Med | Low | When a Reviewer returns Err, the bundle is still active (not Rejected), so the handback logic keeps work in InReview and a new Reviewer is spawned. If the FSM rejection is systemic, this loops until the Lifeguard kills it after 3 repeated errors. This is acceptable - the Lifeguard is the correct circuit breaker for irrecoverable FSM violations. |
| Too many Errs cause lifeguard escalation | Low | Low | Lifeguard tracks repeated SAME errors. Different transitions failing will not trigger lifeguard. |

## Open Questions

None.

## References

- v0.1.47-v0.1.51: Individual fixes that revealed the pattern
- E2E runs: lua-todo consistently stalls due to these silent failures
