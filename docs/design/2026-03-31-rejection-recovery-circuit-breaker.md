# Design Document: Rejection Recovery and Circuit Breaker

**Author:** Scott Idler + Claude
**Date:** 2026-03-31
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

After a bundle rejection, the coordinator enters an infinite LLM loop because
it cannot see the rejected bundle and confuses work IDs with bundle IDs. This
design fixes state visibility, closes a Lifeguard gap, and hardens coordinator
singleton guarantees.

## Problem Statement

### Background

The E2E run on 2026-03-31 exercised the full pipeline: coordinator planned two
work items, implementers produced code, reviewers evaluated bundles, and the
integrator merged approved work. Work 1 completed flawlessly. Work 2's bundle
was rejected by the reviewer.

### Problem

After bundle rejection, the coordinator entered an infinite loop burning LLM
tokens for ~7 minutes until timeout. Three independent failures conspired:

1. **State visibility gap**: `build_state_summary` filters out Rejected bundles
   (coordinator.rs:108-114). The coordinator saw Work `wk-t4l5h` stuck in
   `InReview` but had no bundle to act on. It guessed IDs, passing `wk-t4l5h`
   to `triage_bundle` which expects `bd-*` prefixes.

2. **Lifeguard blind spot**: The Lifeguard circuit breaker only monitors
   `Err(e)` from `execute_action` (coordinator.rs:1571-1583). Validation
   failures from `triage_bundle` return `Ok(ActionResult::ActionError(...))`,
   which bypasses the Lifeguard entirely. The 23 consecutive wrong-ID calls
   were never detected.

3. **Dual coordinator**: Two coordinator sessions (`ag-jkkog` and `ag-uiyds`)
   ran concurrently, both making the same mistake, doubling token burn.
   Coordinator `max_pool` is already 1 (config.rs:214), and `agent.start`
   enforces it (agent.rs:90-106), but the check is not atomic: daemon
   auto-start and `coordinator.set_goal` (coordinator.rs:380) can both call
   `agent.start` before either session is persisted, bypassing the pool check.

### Goals

- Coordinator self-recovers from bundle rejection without external intervention
- Lifeguard catches validation error loops, not just execution failures
- Single coordinator guarantee with no race window
- Delete uncommitted cross-domain mutation hack in bundle.rs

### Non-Goals

- Distributed lease/TTL (single-machine daemon, not needed)
- Reviewer owning work transitions (wrong domain boundary)
- Changing the reviewer's rejection behavior

## Proposed Solution

### Overview

Four changes, ordered by impact. Phase 1 is the root cause fix and may be
sufficient alone. Phases 2-4 are defense-in-depth hardening.

1. **State summary visibility** - show rejected bundles with actionable directive
1b. **Store rejection reason** - reviewer writes reason to `bundle.verification`
2. **Lifeguard gap** - route `ActionResult::ActionError` through the circuit breaker
3. **Coordinator singleton** - make max_pool check atomic in `agent.start`
4. **Delete hack** - remove uncommitted cross-domain mutation in bundle.rs

### Phase 1: State Summary Visibility

**File:** `src/agents/coordinator.rs`, `build_state_summary_with_sla()`

Add a new section after the existing "Recently Merged Bundles" block
(coordinator.rs:127-155). This follows the same pattern - query bundles with a
specific terminal status, cross-reference with parent work items, and surface
actionable items.

```rust
// Rejected Bundles whose parent Work is still InReview (needs rollback)
{
    let Ok(bundles) = stores.read_bundles() else {
        return summary;
    };
    let Ok(works) = stores.read_works() else {
        return summary;
    };
    let mut rejected: Vec<_> = bundles
        .values()
        .filter(|b| b.status == BundleStatus::Rejected)
        .filter(|b| {
            works
                .get(&b.work_id)
                .map(|w| w.status == WorkStatus::InReview)
                .unwrap_or(false)
        })
        .collect();
    rejected.sort_by_key(|b| b.created_at);
    if !rejected.is_empty() {
        summary.push_str("### Rejected Bundles (Work needs reset to Ready)\n");
        for b in &rejected {
            let reason = if b.verification.is_empty() {
                "bundle was rejected by reviewer".to_string()
            } else {
                b.verification.clone()
            };
            summary.push_str(&format!(
                "- [{}] REJECTED (work: {} is InReview, reason: {}) \
                 ACTION: use override_work on {} with target_status ready \
                 and reason 'bundle {} rejected'. \
                 The worker pool will auto-assign a new implementer.\n",
                b.id, b.work_id, reason, b.work_id, b.id
            ));
        }
        summary.push('\n');
    }
}
```

**Why `override_work` to `Ready` instead of `transition` to `InProgress`:**
The work transition handler requires an assignee for `InProgress`
(work.rs:371). While the Work retains its assignee from the first assignment,
resetting to `Ready` is architecturally cleaner: it clears the assignment and
lets the worker pool auto-assign a fresh implementer via `pull_based_workers`.
`override_work` also releases locks and stops any lingering sessions as a
safety measure. `InReview -> Ready` is a valid override transition
(work.rs:140-143).

**Why this works:** The coordinator already successfully handles "Recently
Merged Bundles (WI needs advancing)" using the identical pattern. The LLM
reliably acts on these directives because it has the exact IDs and the exact
action to take. The E2E failure happened precisely because the LLM lacked this
information - it knew the Work was stuck but had no bundle ID and no clear
recovery path.

### Phase 1b: Store Rejection Reason on Bundle

**File:** `src/agents/reviewer.rs`, `ReviewVerdict::Reject` handler (~line 221)

Currently the reviewer only sets `bundle.verification` on approval
(reviewer.rs:185). On rejection, `verification` is left empty. Without the
rejection reason, the next implementer has no context on why the previous
attempt failed and is likely to produce the same rejected code.

Fix: set `verification` on rejection, matching the approval pattern:

```rust
ReviewVerdict::Reject => {
    // Store the rejection reason BEFORE transitioning
    let _ = self.ctx.bridge.request(
        "bundle.update",
        serde_json::json!({
            "id": self.bundle_id,
            "verification": format!("Rejected: {}", review.summary),
        }),
    );
    let resp = self.ctx.bridge.request(
        "bundle.transition",
        serde_json::json!({
            "id": self.bundle_id,
            "target_status": "Rejected",
            "role": "reviewer",
        }),
    );
    // ... existing error handling
}
```

This feeds into Phase 1's state summary directive, which includes the
`verification` field as the rejection reason. The coordinator and subsequent
implementer both benefit from knowing *why* the bundle was rejected.

### Phase 2: Lifeguard Coverage for ActionResult::ActionError

**File:** `src/agents/coordinator.rs`, action execution loop (~line 1569)

Currently the Lifeguard only sees errors from the `Err` branch:

```rust
let result = match execute_action(action_ref, &self.ctx, repo_root, None).await {
    Ok(r) => r,           // <-- ActionError goes here, bypasses Lifeguard
    Err(e) => {           // <-- Lifeguard only checks this branch
        let (verdict, warning) = guard.record_error(&err_msg);
        ...
    }
};
```

Fix: after the match, check if `result` is `ActionError` and route it through
the Lifeguard:

```rust
let result = match execute_action(...).await {
    Ok(r) => r,
    Err(e) => {
        // existing Lifeguard check for hard errors
        ...
        ActionResult::ActionError(err_msg)
    }
};

// NEW: also route ActionError results through the Lifeguard
if let ActionResult::ActionError(ref err_msg) = result {
    let (verdict, warning) = guard.record_error(err_msg);
    if let Some(w) = warning {
        self.ctx.warn(&w);
    }
    if let Verdict::Escalate(reason) = verdict {
        self.ctx.warn(&format!("lifeguard: {}", reason));
        return Ok(IterationOutcome::NeedHelp(format!(
            "lifeguard: tool validation loop (not a system failure): {}",
            reason
        )));
    }
}
```

With the existing Lifeguard thresholds (error_threshold: 3, window: 10), the
23-call loop from the E2E would have been caught after 3 identical
`"triage_bundle: 'wk-t4l5h' is not a bundle ID"` errors.

### Phase 3: Atomic Coordinator Singleton

**File:** `src/daemon/handlers/agent.rs`, `handle_agent_start()`

The existing `max_pool` enforcement (agent.rs:90-106) reads sessions, counts
active coordinators, and rejects if `>= max_pool` (default: 1). But this
read-then-create is not atomic. The E2E dual-coordinator happened because:

1. Daemon auto-start calls `agent.start` for coordinator (mod.rs:253)
2. `loopr run` calls `coordinator.set_goal`, which also calls `agent.start`
   (coordinator.rs:380)
3. Both read "0 active coordinators" before either writes its session

Fix: make the max_pool check and session creation atomic by holding the write
lock on `agent_sessions` across both operations.

```rust
// In handle_agent_start, replace the read-check-then-later-write with:
{
    let mut sessions = stores.write_agent_sessions()?;
    let active_count = sessions
        .values()
        .filter(|s| s.agent_type == agent_type && !s.status.is_terminal())
        .count();
    let max_pool = super::max_pool_for(agent_type, &stores.config) as usize;
    if active_count >= max_pool {
        return Ok(DaemonResponse::err(req.id, RpcError::pool_exhausted(...)));
    }
    // Insert the new session while still holding the write lock
    sessions.insert(session.id.clone(), session);
}
```

This is the simplest correct fix. No new flags, no lease TTLs. The write lock
on `agent_sessions` serializes all `agent.start` calls, making the
check-and-create atomic. The supervisor's separate `has_active_coordinator`
check (supervisor.rs:95-102) becomes redundant but harmless.

**Deadlock safety:** The callers of `handle_agent_start` (daemon auto-start in
mod.rs:253 and `coordinator.set_goal` in coordinator.rs:380) both go through
`dispatch()`, which does not hold any read locks on `agent_sessions`. Upgrading
to a write lock in the handler is safe.

### Phase 4: Delete bundle.rs Cross-Domain Hack

**File:** `src/daemon/handlers/bundle.rs`, lines 429-448 (uncommitted)

Delete the entire block that directly mutates Work status when a bundle is
rejected. With Phase 1 in place, the coordinator handles this transition
through the proper FSM path.

## Alternatives Considered

### Alternative 1: Event Reactor (Choreography)

- **Description:** Background reactor listens to `DaemonEvent::transition_completed`
  for `Bundle -> Rejected`, automatically dispatches `work.transition` to move
  Work back to InProgress.
- **Pros:** Fully decoupled, deterministic, no LLM involvement in recovery.
- **Cons:** Adds a new daemon subsystem. The coordinator still needs to know
  the Work is back in InProgress to assign a new implementer - so it still
  needs the state visibility fix. Solving half the problem with a reactor and
  the other half with state visibility is more complex than solving all of it
  with state visibility alone.
- **Why not chosen:** The coordinator already handles the analogous "merged
  bundle needs Work advanced" case successfully. Rejection recovery is the same
  pattern. Adding a reactor is unnecessary complexity.

### Alternative 2: Reviewer Owns Both Transitions

- **Description:** Reviewer agent calls both `bundle.transition -> Rejected`
  and `work.transition -> InProgress` in sequence.
- **Pros:** Immediate, no coordinator involvement needed.
- **Cons:** If reviewer crashes between the two calls, the system is in the
  same desynced state. Wrong domain boundary - the reviewer reviews code, it
  should not manage work lifecycle.
- **Why not chosen:** Fragile and violates domain ownership.

### Alternative 3: Keep the bundle.rs Handler Hack

- **Description:** Commit the existing uncommitted code that mutates Work
  status inside the bundle transition handler.
- **Pros:** Deterministic, immediate, no LLM involvement.
- **Cons:** Bypasses the Work FSM validation layer. Cross-domain mutation -
  the Bundle handler should not own Work lifecycle. Sets a precedent for
  handler-level coupling that makes the system harder to reason about.
- **Why not chosen:** The proper fix (state visibility) is equally simple and
  maintains clean domain boundaries.

## Technical Considerations

### Dependencies

No new crates. All changes are internal to existing modules.

### Performance

Phase 1 adds one additional read of bundles and works in
`build_state_summary`. These are already read in the same function for other
sections - the data is in memory. Negligible cost.

Phase 2 adds one hash + comparison per `ActionError` result. Negligible.

Phase 3 adds an `exclusive` check to session creation - a single iteration
over in-memory sessions under an existing lock. Negligible.

### Testing Strategy

- **Phase 1:** Unit test: given stores with a Rejected bundle (with and
  without verification) and its Work in InReview, assert `build_state_summary`
  output contains the rejected bundle section with correct IDs, rejection
  reason, and `override_work` directive.
- **Phase 1b:** Unit test: mock a reviewer reject flow, assert
  `bundle.verification` is set to `"Rejected: {summary}"` before the
  transition to Rejected.
- **Phase 2:** Unit test: feed 3 identical `ActionResult::ActionError` values
  through the post-execution Lifeguard check, assert Escalate verdict.
- **Phase 3:** Unit test: call `handle_agent_start` for coordinator twice in
  sequence under the same write lock scope, assert second call returns
  `pool_exhausted` error.
- **E2E:** Re-run `bin/e2e.sh` and verify the coordinator successfully
  transitions the rejected Work back to InProgress and assigns a new
  implementer, reaching GoalComplete (exit code 0) or at least progressing
  past the rejection loop.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| State summary becomes too long with rejected bundles | Low | Low | Only show rejected bundles whose parent Work is still in InReview (self-clearing) |
| Lifeguard too aggressive - escalates on transient errors | Low | Med | Existing threshold (3 in window of 10) is already tuned; ActionError routing uses the same thresholds |
| Coordinator incorrectly transitions Work after rejection | Low | Med | FSM validates the transition; InReview -> InProgress is explicitly allowed for Coordinator role |
| Write lock held longer during agent.start | Low | Low | Lock scope only extends to include session insert; all other callers already tolerate lock contention |

## Edge Cases

### Multiple rejections of the same Work

If a Work is rejected, rolled back to InProgress, re-implemented, and rejected
again, the state summary will show only the *latest* rejected bundle (the
previous one's Work was no longer InReview when it was rejected). The filter is
self-clearing: once the coordinator transitions the Work back to InProgress,
the rejected bundle disappears from the directive section.

### Rejection reason availability

Phase 1b ensures `bundle.verification` is populated on rejection. The state
summary directive includes this reason so the coordinator and next implementer
know *why* the bundle was rejected. If `verification` is empty (e.g., from a
bundle rejected before Phase 1b is deployed), the directive falls back to a
generic "bundle was rejected by reviewer" message.

### Coordinator transition vs override_work

For rejection rollback, the generic `transition` action is sufficient because
no implementer session is active when Work is in InReview. If a future code
path creates a scenario where an active session exists on the Work during
rejection rollback, `override_work` would be needed instead. The current
design is correct for the known flow.

## Open Questions

- [ ] Should we cap the number of rejection cycles per Work item (e.g., reject
      3 times then Abandon), or is the existing `max_work_retries` on
      implementer assignment sufficient?

## References

- E2E session summary: `~/.local/share/loopr/sessions/latest/summary.md`
- E2E script: `bin/e2e.sh`
- Design doc: `docs/design/2026-02-25-orchestration-spine.md` (FSM, TaskStore)
- Design doc: `docs/design/2026-02-26-multi-level-rwl.md` (Coordinator role)
