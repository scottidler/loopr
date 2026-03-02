# Design Document: Coordinator-Driven Integrated → Done Transition

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

The coordinator FSM gets stuck in the `Executing` state because no actor transitions Work items from `Integrated` to `Done`. The integrator correctly parks Work at `Integrated` after merge+validation, but the coordinator — the rightful owner of lifecycle completion — never acknowledges it. This doc proposes making the coordinator explicitly transition `Integrated` Works to `Done` during execution.

## Problem Statement

### Background

Loopr's Work lifecycle follows a multi-actor FSM:

```
Draft → Ready → InProgress → InReview → Integrated → Done
  ↘       ↘        ↘            ↘           ↘
                     Abandoned (from any non-terminal)
```

The transition rules assign clear ownership:
- `InReview → Integrated`: Integrator (code merged, tick validated)
- `Integrated → Done`: Coordinator or Integrator (business acknowledgement)

The `Integrated` state exists as a **handoff point** — a deliberate boundary between the integrator's domain (source control, CI, merge) and the coordinator's domain (process orchestration, lifecycle management).

### Problem

After the integrator publishes a tick and transitions Work to `Integrated`, the coordinator enters a spin loop:

1. `check_fsm_transition()` reads Work status → sees `Integrated` (not terminal)
2. FSM stays in `Executing` (terminal = `Done | Abandoned` only)
3. LLM sees `Integrated` in state summary, sees the footer instruction "Transition Integrated Works to Done"
4. LLM either doesn't emit the transition action, or the action gets lost in a larger response
5. Repeat indefinitely

Meanwhile, dependent Works see their dependency as `Integrated` (not `Done`) in `build_phase_status` at line 830, so they report as `BLOCKED` even though the work is effectively complete.

### Observed Behavior (test-run output)

From the test-run results, the coordinator logged 11+ iterations stuck in `Executing`:

```
[coordinator] iteration 11 failed (will retry): failed to parse agent actions from LLM response
(response snippet: Looking at the current state: 1. Work 01KJNVJCMK9R4EJS7EM5ASX0BW is InReview
with a Triaged bundle... 2. Work 01KJNVJCMMY6C1C1B0A59RET4W is Ready but depends on
01KJNVJCMK9R4EJS7EM5ASX0BW which is still InReview (not Done), so it's effectively blocked...
There's nothing actionable right now)
```

The coordinator sees stale state (`InReview`) in some reads and fresh state (`Integrated`) in others, but never acts on either.

### Goals

- Coordinator transitions `Integrated` → `Done` deterministically, without relying on LLM action emission
- Dependent Works unblock immediately when their dependency is `Done`
- The `Integrated` state remains meaningful as the integrator's handoff marker
- No changes to the integrator's responsibilities

### Non-Goals

- Removing the `Integrated` state from the FSM
- Adding validation logic to the `Integrated → Done` transition (future extension point)
- Changing the integrator to skip `Integrated` and go directly to `Done`

## Proposed Solution

### Overview

Add a deterministic sweep in the coordinator's `run_iteration` that transitions all `Integrated` Work items to `Done` before the LLM is consulted. This runs every iteration during the `Executing` state, ensuring no `Integrated` Work persists across iterations.

### Architecture

The fix has two parts:

**Part 1: Deterministic `Integrated → Done` sweep in the coordinator**

In `run_iteration()`, after the FSM transition check and before building the state summary, sweep all Works in the current phase and transition any `Integrated` items to `Done`:

```rust
// In run_iteration(), after check_fsm_transition() block (line ~1160):

// Deterministic: transition Integrated Works to Done.
// The integrator parks Work at Integrated after merge+validation;
// the coordinator acknowledges completion.
if coord_state.fsm_state == CoordinatorFsmState::Executing {
    if let Some(ref phase_id) = coord_state.current_phase_id {
        let integrated_ids: Vec<String> = {
            let works = stores.works.read().unwrap();
            works
                .values()
                .filter(|w| w.phase_id == *phase_id && w.status == WorkStatus::Integrated)
                .map(|w| w.id.clone())
                .collect()
        };
        for wi_id in &integrated_ids {
            let resp = self.ctx.bridge.request(
                "work.transition",
                serde_json::json!({
                    "id": wi_id,
                    "target_status": "Done",
                    "role": "coordinator",
                }),
            );
            if resp.is_error() {
                self.ctx.warn(&format!(
                    "failed to transition WI {} Integrated→Done: {:?}",
                    wi_id, resp.error
                ));
            } else {
                self.ctx.info(&format!("Work {} transitioned Integrated → Done", wi_id));
            }
        }
    }
}
```

**Part 2: Re-check FSM after the sweep**

After transitioning Works to `Done`, re-run `check_fsm_transition()` so the FSM can immediately advance to `PhaseGate` if all Works are now terminal. Without this, the coordinator would waste one full iteration before noticing all Works are `Done`.

The existing FSM transition handling (lines 1113-1159) must be extracted into a reusable `apply_fsm_transition()` helper. The current code handles `ActivatePhase` (find next phase, set context), `PhaseGate` (mark phase complete), `GoalComplete` (deactivate goal, return early), and default (simple state change). This helper is called in two places:

```rust
// Simplified run_iteration() flow:

// 1. Initial FSM check (existing)
if let Some(new_state) = check_fsm_transition(stores, coord_state, config) {
    if let Some(outcome) = apply_fsm_transition(new_state, coord_state, stores, config)? {
        return Ok(outcome);  // GoalComplete returns early
    }
}

// 2. Integrated → Done sweep (new, only during Executing)
sweep_integrated_to_done(stores, coord_state, bridge, &self.ctx)?;

// 3. FSM re-check after sweep (new)
if let Some(new_state) = check_fsm_transition(stores, coord_state, config) {
    if let Some(outcome) = apply_fsm_transition(new_state, coord_state, stores, config)? {
        return Ok(outcome);
    }
}

// 4. Build state summary and call LLM (existing)
let state_summary = build_state_summary_with_sla(...);
```

### Why Deterministic, Not LLM-Driven

The LLM is already instructed to emit `transition` actions for `Integrated → Done` (line 724 in `build_fsm_footer`). It doesn't reliably do so because:

1. The LLM sometimes responds with prose instead of JSON (parse failures)
2. The LLM sees stale state in some iterations and focuses on other concerns
3. The transition is a mechanical acknowledgement, not a judgment call

Making it deterministic means:
- Zero wasted iterations waiting for the LLM to notice
- No parse-failure risk for a trivial state change
- Consistent behavior regardless of LLM response quality

### Data Model

No changes. The `WorkStatus::Integrated` and `WorkStatus::Done` states already exist with the correct transition rules.

### Implementation Plan

1. Extract the existing FSM transition handling (lines 1113-1159) into a `apply_fsm_transition()` helper. The current block is 50+ lines handling `ActivatePhase`, `PhaseGate`, `GoalComplete`, and default cases. This helper is called in two places: the existing pre-iteration check and the new post-sweep re-check.
2. Add the `Integrated → Done` sweep in `coordinator.rs:run_iteration()` after the initial FSM check block, before `build_state_summary_with_sla()`.
3. Call `apply_fsm_transition()` after the sweep so the FSM can immediately advance to `PhaseGate` if all Works are now terminal.
4. Remove the "Transition Integrated Works to Done" instruction from the `Executing` state footer (line 724) since it's now deterministic.
5. Add tests verifying the sweep and FSM re-check behavior.

## Alternatives Considered

### Alternative A: Integrator transitions directly to Done

- **Description:** The integrator steps through `InReview → Integrated → Done` in a single operation after publishing the tick.
- **Pros:** Simple, no coordinator changes needed.
- **Cons:** Collapses `Integrated` into a transient ghost-state with no observability or extension value. The integrator takes on lifecycle responsibilities outside its domain. If the definition of "Done" changes in the future (e.g., requires staging deployment verification), the integrator must be modified.
- **Why not chosen:** Violates separation of concerns. The integrator's domain is source control and CI, not business lifecycle.

### Alternative B: FSM treats Integrated as terminal

- **Description:** Add `WorkStatus::Integrated` to the terminal check in `check_fsm_transition()` alongside `Done` and `Abandoned`.
- **Pros:** One-line change, immediately unblocks the FSM.
- **Cons:** Makes `Integrated` and `Done` semantically identical for phase completion. Every downstream query or metric that checks for terminal state must now branch on `Done OR Integrated`. Dependency satisfaction (`build_phase_status` line 830) would also need updating, and any future code that checks for terminal status must remember to include `Integrated`.
- **Why not chosen:** Semantic leak. Two terminal success states create branching logic nightmares across the codebase.

### Alternative C: LLM-only (current behavior + better prompting)

- **Description:** Improve the coordinator prompt to more aggressively instruct the LLM to transition `Integrated` Works.
- **Pros:** No code changes.
- **Cons:** Unreliable. LLM responses are non-deterministic; parse failures, stale state reads, and attention drift mean the transition may take many iterations or never happen. A mechanical acknowledgement should not depend on LLM reliability.
- **Why not chosen:** The `Integrated → Done` transition is not a judgment call. Deterministic behavior is correct here.

## Technical Considerations

### Dependencies

- `src/agents/coordinator.rs` — sweep logic, FSM helper extraction
- `prompts/coordinator.pmt` — remove stale LLM instruction (step 4)
- No new external dependencies

### Performance

Negligible. The sweep reads the works lock once per iteration (already happening) and makes 0-N IPC calls for `Integrated` items. In practice, N is 1-2 per phase.

### Testing Strategy

1. Unit test: coordinator sweep transitions `Integrated` Works to `Done`
2. Unit test: FSM re-check advances to `PhaseGate` after sweep clears all Works
3. Unit test: sweep is a no-op when no Works are `Integrated`
4. Integration test: full pipeline — Work goes `InReview → Integrated` (by integrator) → `Done` (by coordinator sweep) → FSM advances

### Rollout Plan

Direct code change. No feature flag needed — this is a bug fix for deterministic behavior that was always intended.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coordinator transitions before integrator finishes | Low | Medium | The integrator transitions to `Integrated` only after tick is `Published`. The coordinator only sweeps Works already in `Integrated` state. |
| Future "Done" requirements need a gate | Low | Low | The sweep is the extension point. Add validation logic inside the sweep before transitioning. |
| Duplicate transition attempts (LLM + sweep) | Medium | None | IPC handler is idempotent — transitioning `Done → Done` is a no-op or benign error. Remove the LLM instruction to reduce noise. |
| IPC failure during sweep | Low | Low | Sweep logs warning and continues. Next iteration retries automatically since the FSM won't advance until all Works are terminal. Natural retry with zero extra code. |

## Open Questions

- [x] ~~Should the `build_phase_status` dependency check (line 830) also accept `Integrated` as satisfying a dependency?~~ No — the sweep runs before the state summary is built, so by the time `build_phase_status` reads Works, they're already `Done`. No change needed.
- [x] ~~Should the LLM footer instruction "Transition Integrated Works to Done" be removed?~~ Yes — moved to implementation plan step 4. Keeping the instruction would cause duplicate transition attempts (deterministic sweep + LLM action) which adds noise to logs.

## References

- `src/agents/coordinator.rs` — FSM check (line 912-928), `run_iteration` (line 1099), `build_fsm_footer` (line 718-728), `build_phase_status` (line 776-862)
- `src/agents/integrator.rs` — Work transition to `Integrated` (line 657-694)
- `src/domain/work.rs` — `WorkStatus` enum and transition rules (line 11-107)
- `src/agents/executor.rs` — `Transition` action execution (line 421+)
- `prompts/coordinator.pmt` — LLM instructions (line 19, 38)
