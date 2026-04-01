# Design Document: Merged Bundle Override Guard

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 4/4

## Summary

A race condition exists between the Coordinator's `OverrideWork` action and the Integrator's merge flow. When the Coordinator resets a Work item to `Ready` based on a stale rejection while the Integrator has already merged a newer bundle, the code lands on `main` but the Work is stuck in a doom loop at `Ready`. This document adds a pre-execution guard in the `OverrideWork` handler that rejects overrides to `Ready` when any bundle for the target Work has `BundleStatus::Merged` or `BundleStatus::Integrating`.

## Problem Statement

### Background

The Coordinator uses `OverrideWork` (introduced in `2026-03-01-coordinator-override-sla-recovery.md`) to recover stuck Work items. The override FSM rules in `src/domain/work.rs:120-160` permit `InReview -> Ready` as a valid coordinator override, enabling reset when no valid bundle exists.

The Integrator operates on a separate async loop, processing accepted bundles through build verification and merge. Bundle and Work transitions happen as separate RPC calls with a timing gap between them.

### Problem

The race unfolds in five steps:

1. **Work is InReview** with the latest bundle (e.g., `bd-htxeg`) actively being processed by the Reviewer/Integrator pipeline.
2. **Coordinator sees a stale rejection.** The Coordinator's `build_state_summary()` (`src/agents/coordinator.rs:170-199`) scans for rejected bundles whose parent Work is still `InReview`. It finds an older rejected bundle (e.g., `bd-qf3pf`) and prescribes `OverrideWork { target_status: "Ready" }`.
3. **The FSM approves the override.** `InReview -> Ready` is a legal override transition (`src/domain/work.rs:142-146`). The Work state is mutated back to `Ready`.
4. **The Integrator succeeds.** The Integrator finishes verifying `bd-htxeg`, marks it `Merged`, and attempts to transition the Work to `Integrated`.
5. **The desync.** The Work is at `Ready` (not `InReview`), so the Integrator's `InReview -> Integrated` transition fails. The code is on `main`, but the task store thinks the Work needs to start over. A doom loop begins - new implementers are assigned to work that's already done.

**Root cause:** The `OverrideWork` executor (`src/agents/executor.rs:1637-1732`) has no awareness of bundle state. It blindly forwards the transition to the FSM, which only validates `(from_status, to_status, role)` - not whether the Work has irreversible progress (merged code).

### Goals

- Reject `OverrideWork` actions that would reset a Work item when any of its bundles have been merged
- Return a clear error message to the Coordinator LLM so it can adjust its plan
- No changes to the FSM transition rules themselves - the guard is a precondition check in the handler

### Non-Goals

- Fixing the Coordinator's `build_state_summary()` to filter out stale rejections (separate concern - the guard is the safety net regardless of prompt quality)
- Adding distributed locks or two-phase commits between Coordinator and Integrator
- Changing the Integrator's merge flow
- Guarding non-Ready override targets (overriding to `Abandoned` when a bundle is merged is fine - it's the reset-to-Ready that causes the doom loop)

## Proposed Solution

### Overview

Add a bundle-status precondition check in the `OverrideWork` handler in `src/agents/executor.rs`, before the `work.transition` RPC call. If the target status is `Ready` and any bundle for the Work has `BundleStatus::Merged` or `BundleStatus::Integrating`, reject the action with an `ActionResult::ActionError` containing a descriptive message. Checking `Integrating` as well prevents the override from racing with an in-flight merge - if the build later fails, the next Coordinator cycle will see only Rejected bundles and the override will proceed.

### Implementation

**File:** `src/agents/executor.rs`, lines 1637-1732 (OverrideWork arm)

Insert the guard at the top of the OverrideWork arm, BEFORE killing active sessions (line 1643). This ensures no side effects (session kills, lock releases) occur when the override is rejected:

```rust
// Guard: reject override-to-Ready if any bundle is Merged or Integrating
if target_status == "Ready" {
    let bundles = bridge.stores().read_bundles()?;
    let blocking = bundles.values().find(|b| {
        b.work_id == *work_id
            && matches!(
                b.status,
                crate::domain::bundle::BundleStatus::Merged
                    | crate::domain::bundle::BundleStatus::Integrating
            )
    });
    let blocked = blocking.map(|b| (b.id.clone(), b.status));
    drop(bundles);
    if let Some((bundle_id, status)) = blocked {
        let msg = format!(
            "Cannot override Work {} to Ready: bundle {} is {:?}. \
             Code is merged or merge is in flight. \
             Do not retry this override - let the Integrator finish.",
            work_id, bundle_id, status
        );
        agent_log.warn(&format!("OverrideWork: {}", msg));
        return Ok(ActionResult::ActionError(msg));
    }
}
```

**Why in the executor, not the FSM handler:**

The FSM handler (`src/daemon/handlers/work.rs:298-417`) validates transitions generically. Adding bundle-aware logic there would entangle domain knowledge about the override-merge race into the generic transition machinery. The executor is where action-specific preconditions belong - it already checks sessions and locks.

**Why not in `build_state_summary()`:**

Defense in depth. Even if we fix the Coordinator's prompt to avoid recommending stale overrides, a belt-and-suspenders guard in the executor prevents the race from ever succeeding. The Coordinator is an LLM - it will occasionally make bad calls. The executor guard is deterministic.

### Error Message Design

The error message explicitly tells the LLM:
1. What happened: which bundle is blocking and its status (Merged or Integrating)
2. Why it matters: "Code is merged or merge is in flight"
3. What to do instead: "Do not retry this override - let the Integrator finish"

This gives the Coordinator LLM enough context to back off. It does NOT suggest "transition to Done" because the Work is still at `InReview` when the guard fires - the correct resolution is for the Integrator to complete its `InReview -> Integrated -> Done` flow naturally.

### Data Flow

```
Coordinator LLM
  -> OverrideWork { work_id, target_status: "Ready", reason }
    -> executor.rs: [NEW] check bundles for Merged/Integrating status
      -> if blocked: return ActionError (no side effects, LLM backs off)
    -> executor.rs: kill sessions
    -> executor.rs: work.transition RPC
    -> executor.rs: release locks, create audit Learning
```

## Alternatives Considered

### Alternative 1: Fix build_state_summary() filtering

- **Description:** Filter out rejected bundles from the state summary when a newer non-rejected bundle exists for the same Work.
- **Pros:** Prevents the Coordinator from ever recommending the bad override.
- **Cons:** Prompt-level fix only - the LLM could still hallucinate an override. Does not protect against future code paths that might call OverrideWork.
- **Why not chosen:** Should be done as a follow-up improvement, but is not sufficient as the sole fix. The executor guard is the safety net.

### Alternative 2: Guard in the FSM handler (work.rs transition handler)

- **Description:** Add the merged-bundle check inside `handle_work_transition()` when `is_override && target_status == Ready`.
- **Pros:** Catches all callers, not just the executor.
- **Cons:** Mixes domain-specific race-condition knowledge into generic FSM machinery. The FSM handler already has precondition checks (#13 assignee, #14 acceptance_criteria, #15 active bundle), but those are structural invariants - not race-condition guards. The override-merge race is an executor-level concern.
- **Why not chosen:** Executor-level guard is architecturally cleaner. If a future non-executor caller needs the same guard, it can be promoted to the handler then.

### Alternative 3: Two-phase lock between Coordinator and Integrator

- **Description:** Acquire a lock on the Work item before any override or merge transition; both agents check the lock.
- **Pros:** Solves the general class of concurrent-mutation races.
- **Cons:** Significant complexity. The existing lock system is for file-level locks in worktrees, not record-level FSM locks. Would require a new lock type, deadlock handling, and changes to both agents.
- **Why not chosen:** Massively over-engineered for this specific race. The bundle-status guard is sufficient because merged-ness is a permanent, monotonic state - once a bundle is Merged, it stays Merged. No lock is needed to read a stable fact.

## Technical Considerations

### Dependencies

None. Uses existing `bridge.stores().read_bundles()` which is already available in the executor context.

### Performance

One additional `read_bundles()` call per `OverrideWork` execution. OverrideWork is rare (only on SLA breach recovery), so the impact is negligible.

### Testing Strategy

1. **Unit test - Merged blocks Ready:** Create a Work with a Merged bundle, attempt OverrideWork to Ready, assert ActionError is returned with bundle ID in message.
2. **Unit test - Integrating blocks Ready:** Create a Work with an Integrating bundle, attempt OverrideWork to Ready, assert ActionError is returned.
3. **Unit test - Rejected allows Ready:** Create a Work with only Rejected bundles, attempt OverrideWork to Ready, assert transition proceeds.
4. **Unit test - non-Ready target bypasses guard:** Create a Work with a Merged bundle, attempt OverrideWork to Abandoned, assert transition proceeds (the guard only blocks Ready).
5. **Unit test - mixed bundles:** Create a Work with one Rejected and one Merged bundle, attempt OverrideWork to Ready, assert ActionError (Merged takes precedence).

### Rollout

Single code change in `executor.rs`. No migration, no config, no flag. Ships with the next build.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Guard blocks a legitimate override-to-Ready after a false Merged status | Very Low | Medium | BundleStatus::Merged is set by the Integrator only after a verified git merge. False positives are not possible in the current flow. |
| Coordinator enters a loop trying to override, getting rejected each time | Low | Low | The ActionError message tells it to back off and let the Integrator finish. The LLM prompt also has retry limits. |
| Race between guard read and concurrent Integrator merge | Very Low | Medium | The guard narrows but does not eliminate the race window. If a bundle transitions to Integrating after the guard reads but before the override RPC lands, the original race can still occur. However: (a) checking Integrating in addition to Merged shrinks the window to microseconds (read-to-RPC latency), vs. the original seconds-long window; (b) if the race does hit, the next Coordinator cycle will see Ready + Merged bundle and can detect the desync. A follow-up Coordinator prompt improvement to detect "Ready with Merged bundle -> transition to Done" would close this residual gap. |

## Open Questions

- [ ] Should `build_state_summary()` also be patched to filter stale rejections as a follow-up? (Recommended but separate scope)
- [ ] Should the Coordinator prompt include a "Ready with Merged bundle" detector to auto-transition such Work to Done? This would close the residual microsecond race window described in Risks.

## References

- `docs/design/2026-03-01-coordinator-override-sla-recovery.md` - introduced OverrideWork and override transitions
- `src/agents/executor.rs:1637-1732` - OverrideWork handler
- `src/domain/work.rs:120-160` - override_transitions()
- `src/agents/coordinator.rs:170-199` - build_state_summary() stale rejection detection
- `src/agents/integrator.rs:665-719` - bundle merge and Work transition flow
