# Design Document: Superseded State and GoalComplete Correctness

**Author:** Scott A. Idler
**Date:** 2026-04-14
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The coordinator currently declares `GoalComplete` when all works, phases, and specs reach a
terminal state - including `Abandoned`. This allows a run where all work failed to be reported
as successful. This document introduces a `Superseded` terminal state for both `WorkStatus` and
`HierarchyStatus` to distinguish intentionally-replaced items from hard failures, and tightens
`GoalComplete` to require `Done`/`Complete` on all non-Superseded nodes. Two supporting fixes
are included: automatic worktree rebase before bundle proposal (prevents false out-of-scope
deletions in bundle diffs), and graceful handling of the `Failed -> Published` tick FSM error
in the integrator.

## Problem Statement

### Background

The orchestration hierarchy is Plan -> Spec -> Phase -> Work. The coordinator drives execution
through this hierarchy using a reactive reconciler. When work fails repeatedly, the coordinator
abandons it and may create replacement work. The reconciler's completion passes use "all
terminal" as the signal to mark a Phase or Spec complete, where terminal includes both `Done`
and `Abandoned`.

### Problem

Three bugs were identified from the 2026-04-14 `python-api` E2E run:

**Bug 1: GoalComplete fires on Abandoned hierarchy**

`detect_goal_complete` in `src/agents/coordinator/reconcile.rs:309` uses:

```rust
works.iter().all(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
```

And the full-mode check at line 321:

```rust
plan_specs.iter().all(|s| matches!(s.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned))
```

Similarly, `all_children_terminal_works` (line 450) and `all_children_terminal_phases` (line 465)
both count `Abandoned` as satisfying the completion gate. This means a plan where all work
was abandoned will report `GoalComplete = true`.

There is no distinction in the data model between:
- A work item abandoned because the coordinator replaced it with a better-scoped one
- A work item abandoned because all recovery attempts failed

**Bug 2: Bundle diffs show out-of-scope file deletions**

Implementer worktrees are created from the current `integration/<plan_id>` branch tip at
session start. When multiple sessions run in parallel and one merges a bundle (advancing the
integration branch), other still-running sessions no longer reflect the integration branch tip.
When those sessions propose a bundle, the diff computed against the current integration branch
shows files added by earlier-merging work as "deleted."

In the python-api run: `wk-meapl` and `wk-j6iyx` bundles were repeatedly rejected because
they appeared to delete `database.py` and gut `main.py` - files that were added to the
integration branch after those sessions started.

**Bug 3: Tick FSM `Failed -> Published` error**

When a merge fails, the integrator correctly transitions the tick to `Failed` (line 662).
However under certain conditions (daemon restart recovery via `recover_stuck_ticks`, or a
racing publish call), the integrator subsequently attempts to publish the same tick, triggering
`transition rejected: invalid transition from Failed to Published`. This RpcError propagates as
a hard failure and leaves bundles stuck in `Integrating` state, forcing the coordinator to
override works back to `Ready` and discard approved implementations.

### Goals

- `GoalComplete` only fires when all non-Superseded nodes in the hierarchy are `Done`/`Complete`
- The coordinator can mark a work/phase/spec as `Superseded` when intentionally replacing it,
  preserving the distinction from hard `Abandoned` (no replacement, recovery failed)
- Bundle diffs reflect only what the implementer changed, not files added to the integration
  branch while the session was running
- The integrator handles the `Failed -> Published` tick transition gracefully without
  propagating it as a hard failure

### Non-Goals

- Changing how the coordinator decides whether to replace vs abandon work
- Acceptance-criteria-level coverage analysis (verifying that a Superseded work's ACs are
  covered by its replacement)
- Altering the tick FSM state machine definitions

## Proposed Solution

### Overview

Three targeted changes:

1. Add `Superseded` to `WorkStatus` and `HierarchyStatus`. Update FSM YAML, Rust enums,
   `FsmStatus` impls, and the reconciler to ignore Superseded nodes in completion gates.
   Update coordinator prompts to use `Superseded` when replacing.

2. Add a rebase step in the `propose_bundle` primitive handler: before computing the diff,
   rebase the implementer's branch onto the current integration tip.

3. In the integrator, guard the publish call with a pre-publish state check.

### Architecture

#### 1. Superseded State

**New invariants:**

- `Superseded` = coordinator intentionally replaced this node before it reached a hard failure;
  a newer sibling carries its semantic intent forward
- `Abandoned` = coordinator's explicit judgment that recovery is impossible; human intervention
  required; permanently blocks GoalComplete

**Critical control flow rule: only the coordinator can abandon.**

The max-attempts ceiling must NOT auto-transition a work to `Abandoned`. Instead, when
max-attempts is reached, the system transitions the work to `Blocked` and emits a critical
NeedHelp signal to the coordinator. This ensures the coordinator always has the final say on
whether a failure is recoverable (Superseded) or fatal (Abandoned).

When a work hits max-attempts and transitions to `Blocked` with a NeedHelp signal, the
coordinator evaluates and takes one of two actions:

1. **Recoverable:** Coordinator determines the work is malformed, calls `override_work` with
   `target_status: "Superseded"`, and creates a replacement Work in the same Phase
2. **Fatal:** Coordinator determines recovery is impossible, calls `override_work` with
   `target_status: "Abandoned"` - Phase hangs, human intervention required

This eliminates the timing race entirely. No background daemon rule can bypass the coordinator
to unilaterally kill a work item. The coordinator is always guaranteed the final say.

**Coordinator lifecycle flow:**

1. Work cycles through session/bundle failures (Ready -> InProgress -> Blocked -> Ready)
2. Work hits max-attempts -> transitions to `Blocked` + NeedHelp signal
3. Coordinator observes the NeedHelp signal
4. Coordinator calls `override_work` with either `Superseded` (creates replacement) or
   `Abandoned` (declares fatal failure)

`Abandoned` is never reached by automatic system action. It is always an explicit coordinator
judgment.

**FSM transitions added (work.yml):**

```yaml
superseded:
  # terminal, no outbound transitions

# New inbound transitions to superseded:
draft:
  superseded: { by: [coordinator] }
pending:
  superseded: { by: [coordinator] }
ready:
  superseded: { by: [coordinator] }
in-progress:
  superseded: { by: [coordinator] }
blocked:
  superseded: { by: [coordinator] }
in-review:
  superseded: { by: [coordinator] }
```

**FSM transitions added (hierarchy.yml):**

```yaml
superseded:
  # terminal, no outbound transitions

# New inbound transitions:
draft:
  superseded: { by: [coordinator] }
pending:
  superseded: { by: [coordinator] }
active:
  superseded: { by: [coordinator] }
```

**Reconciler changes (`src/agents/coordinator/reconcile.rs`):**

The key insight is that `Abandoned` now means "hard failure, coordinator gave up" and must NOT
satisfy any completion gate. The reconciler separates two distinct checks:

**Dep-terminal check** (`all_hierarchy_deps_terminal`) - used for downstream promotion:
Add `Superseded` as a valid terminal state. Superseded deps do not block promotion of
downstream specs/phases (same as current `Abandoned` behavior). Work deps (`all_work_deps_done`)
are NOT changed: `Superseded` does not satisfy work deps. If Work A is Superseded, any Work
that depends on Work A remains blocked until the coordinator re-wires its deps to point at the
replacement.

**Parent completion gate** - used for bottom-up completion:

- `complete_phases` fires only when:
  - All child Works are `Done | Superseded` (none are Abandoned, InProgress, Ready, etc.)
  - AND at least one child Work is `Done`
- `complete_specs` fires only when:
  - All child Phases are `Complete | Superseded`
  - AND at least one child Phase is `Complete`
- `detect_goal_complete`: Brief mode checks `Done` only; Full mode checks `Complete` only

**Why the "at least one Done" requirement matters:**

If the coordinator supersedes all works in a Phase without creating replacements, all children
would be `Superseded`. `all(Done | Superseded)` is vacuously true. Without the `any(Done)`
guard, the Phase would incorrectly complete. The guard ensures a Phase never completes unless
at least one work actually delivered code.

**Consequence for Abandoned works:**

A Phase with an Abandoned child stays `Active` indefinitely. `Abandoned` is an emergency stop -
it cannot be resolved by the coordinator within the same run. Human intervention is required.
This is intentional: the system surfaces the hard failure visibly rather than silently
swallowing it.

**Macro-level pruning - OverridePhase and OverrideSpec:**

There are cases where the coordinator needs to prune an entire Phase or Spec that was never
the right decomposition - not because the work failed, but because the Phase itself was wrong
(e.g., a Phase for "Deploy to Kubernetes" when the user only asked for a local CLI tool).

For this, the coordinator needs `OverridePhase` and `OverrideSpec` primitives that accept
`target_status: "Superseded"` (or `"Abandoned"` for hard pruning). These are added to the
coordinator's action space alongside the existing `override_work`.

Without these primitives, the coordinator can only manage the bottom layer of the hierarchy.
The full hierarchy (Plan -> Spec -> Phase -> Work) requires management at every level.

**Coordinator prompt update:**

- Use `override_work` with `target_status: "Superseded"` when replacing a failing work item.
  Do this BEFORE it hits max-attempts. Simultaneously create the replacement work.
- Use `override_work` with `target_status: "Abandoned"` only as a last resort when a work
  is truly dead and no replacement is warranted.
- Use `override_phase` with `target_status: "Superseded"` when an entire phase is the wrong
  decomposition and should be pruned wholesale.
- Use `override_spec` with `target_status: "Superseded"` when an entire spec is out of scope
  or structurally wrong.

#### 2. Worktree Rebase Before Bundle Proposal

The `propose_bundle` action handler in `src/agents/executor/action/bundle.rs` currently
computes the diff using `ctx.session.base_ref` (the integration SHA captured at session start)
and creates the bundle without rebasing first.

Add a rebase step in the `propose_bundle` handler before diff computation:

1. Resolve the current integration branch tip (`integration/<plan_id>` or latest published
   tick SHA via `resolve_worktree_base_for`)
2. Run `git rebase <current_tip>` in the worktree path
3. On rebase conflict: return a descriptive tool error so the implementer agent can retry or
   resolve
4. On success: update `ctx.session.base_ref` to the new tip SHA, then proceed with existing
   diff and bundle creation

This is the correct location because the executor action handler has access to both the
session context (worktree path, base_ref) and the stores (to resolve the current integration
tip).

#### 3. Tick Publish Guard

In `src/agents/integrator.rs` around the publish tick call (line 845), add a state check
before publishing:

```rust
// Check tick is still in a publishable state before attempting publish.
let tick_state = {
    let ticks = self.ctx.stores.read_ticks()?;
    ticks.get(&tick_id).map(|t| t.status())
};
if tick_state != Some(TickStatus::Validating) {
    // Tick was transitioned away (e.g. Failed by recover_stuck_ticks).
    // Log and return ValidationFailed instead of erroring hard.
    return Ok(IntegratorCycleResult::ValidationFailed {
        tick_id,
        log: format!("Tick {} not in Validating state before publish (was {:?})", tick_id, tick_state),
    });
}
```

### Data Model

**`src/domain/work.rs` - WorkStatus:**

```rust
pub enum WorkStatus {
    Draft,
    Pending,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Integrated,
    Done,
    Superseded,  // NEW: intentionally replaced by coordinator
    Abandoned,
}
```

**`src/domain/plan.rs` - HierarchyStatus:**

```rust
pub enum HierarchyStatus {
    Draft,
    Pending,
    Active,
    Complete,
    Superseded,  // NEW: intentionally replaced by coordinator
    Abandoned,
}
```

**`src/fsm/status.rs`** - add `Superseded` arms to `to_yaml_name`, `from_yaml_name`,
`all_variants` for both `WorkStatus` and `HierarchyStatus`.

No new fields on `Work` or `Spec`/`Phase` structs. No `replaces` pointer needed - the
coordinator creates the replacement with the same `parent_id`, so the reconciler can verify
"at least one non-Superseded sibling exists" without explicit links.

### API Design

- `override_work`: existing action, now accepts `target_status: "Superseded"` in addition to
  existing values
- `override_phase`: NEW action (`phase_id`, `target_status`, `reason`). Valid targets:
  `Superseded`, `Abandoned`
- `override_spec`: NEW action (`spec_id`, `target_status`, `reason`). Valid targets:
  `Superseded`, `Abandoned`
- No other IPC protocol changes required

### Implementation Plan

#### Phase 1: FSM and Domain Types
**Model:** sonnet

- Add `Superseded` variant to `WorkStatus` in `src/domain/work.rs`
- Add `Superseded` variant to `HierarchyStatus` in `src/domain/plan.rs`
- Update `FsmStatus` for `WorkStatus` in `src/fsm/status.rs`: `to_yaml_name`, `from_yaml_name`,
  `all_variants`
- Update `FsmStatus` for `HierarchyStatus` in `src/fsm/status.rs`: same three methods
- Add `superseded` state and inbound transitions to `strategies/fsm/work.yml`
- Add `superseded` state and inbound transitions to `strategies/fsm/hierarchy.yml`
- Update any exhaustive `match` on `WorkStatus` or `HierarchyStatus` that will fail to compile
- Run `otto check` to confirm compile

#### Phase 2: Reconciler
**Model:** opus

- Update `detect_goal_complete`: Brief mode `WorkStatus::Done` only; Full mode
  `HierarchyStatus::Complete` only
- Update `complete_phases` gate: all children `Done | Superseded` AND at least one `Done`
- Update `complete_specs` gate: all children `Complete | Superseded` AND at least one `Complete`
- Update `all_hierarchy_deps_terminal`: add `Superseded` as terminal alongside `Complete |
  Abandoned` so Superseded deps do not block downstream promotion
- Do NOT change `all_work_deps_done` - `Superseded` does not satisfy work deps; coordinator
  must re-wire deps to the replacement work
- Remove `Abandoned` from all completion gates (phases and specs stay Active with Abandoned
  children, forcing coordinator to act)
- Fix `is_phase_complete` in `src/agents/generation.rs:274` with the same `Done | Superseded`
  + `any(Done)` logic
- Add `Superseded` counter to `src/primitive/catalog/reconcile.rs` summary
- Run `otto test` on reconcile tests

#### Phase 3: Worktree Rebase Before Bundle Proposal
**Model:** sonnet

- In `src/agents/executor/action/bundle.rs`, at the top of the `propose_bundle` handler:
  resolve the current integration branch tip via `resolve_worktree_base_for`
- Run `git rebase <current_tip>` in the worktree path (available via `ctx.session`)
- On conflict: return a descriptive tool error to the implementer agent
- On success: update `ctx.session.base_ref` to the new tip SHA
- Proceed with the existing diff and bundle creation logic
- Run `otto check`

#### Phase 4: Tick Publish Guard
**Model:** sonnet

- In `src/agents/integrator.rs` around the publish tick call: add state check
- If tick is not in `Validating` state, return `ValidationFailed` instead of erroring
- Run `otto check`

#### Phase 5: Max-Attempts Control Flow Change
**Model:** opus

- In `src/daemon/handlers/work.rs:503`: change `WorkStatus::Abandoned` to `WorkStatus::Blocked`
- After persistence (around line 512), emit a NeedHelp learning/event to the coordinator with
  the `work_id`, `attempt_count`, and the last failure reason
- Grep for any other code paths that transition a Work to `Abandoned` without going through
  the coordinator's `override_work` action and reroute them the same way
- The coordinator then evaluates the NeedHelp signal and calls either `Superseded` (creates
  replacement) or `Abandoned` (declares fatal)
- Run `otto check`

#### Phase 6: OverridePhase and OverrideSpec Primitives
**Model:** sonnet

- Add `OverridePhase` action to `src/agents/action.rs` (same shape as `OverrideWork`:
  `phase_id`, `target_status`, `reason`)
- Add `OverrideSpec` action to `src/agents/action.rs` (`spec_id`, `target_status`, `reason`)
- Wire both through `src/primitive/catalog/mutation.rs` and the coordinator bridge
- Valid target statuses: `Superseded`, `Abandoned`
- FSM transition handlers: use existing `force_status` path with role-gated validation
- Run `otto check`

#### Phase 7: Coordinator Prompt Update
**Model:** sonnet

- Update coordinator prompt/guidance (`src/prompts.rs` or `src/guidance.rs`) to describe
  `Superseded` vs `Abandoned` semantics across all hierarchy levels
- Critical instruction: when a NeedHelp signal arrives for a blocked work, evaluate and call
  `Superseded` (recoverable) or `Abandoned` (fatal). `Abandoned` = human intervention required.
- Document `override_phase` and `override_spec` for macro-level pruning
- Run `otto ci`

#### Phase 8: Tests
**Model:** sonnet

- Update `src/tests/fsm/work.rs`: add `Superseded` FSM transition tests
- Update `src/tests/fsm/hierarchy.rs`: add `Superseded` FSM transition tests
- Update `src/agents/coordinator/reconcile/tests.rs`: add tests for GoalComplete with
  Superseded works, mixed Done/Superseded/Abandoned scenarios, and the safety check
- Run `otto ci`

## Alternatives Considered

### Alternative 1: Check Bundle.Merged Instead of Work.Done

- **Description:** GoalComplete checks that every Work has at least one `Merged` bundle
  pointing to it, rather than checking `Work.status`
- **Pros:** Directly verifies delivery; Bundle is the artifact that represents "code landed"
- **Cons:** `Work.Done` and `Bundle.Merged` are already coupled invariants - when a bundle
  merges, the work transitions to Done. The checks are equivalent. No benefit over simply
  tightening the Work.Done gate.
- **Why not chosen:** Redundant with the simpler fix

### Alternative 2: Graph-Aware Replacement Tracing (replaces field)

- **Description:** Add `replaces: Option<WorkId>` to Work. GoalComplete walks the replacement
  chain: a Work is "satisfied" if it is Done, or if it is Abandoned and has a Done descendant
  in its replacement chain.
- **Pros:** Explicit graph; no new state variant
- **Cons:** Graph traversal is complex; cycles are possible; no structural support in the
  current IPC architecture for the coordinator to provide `replacement_work_id` atomically
  with `override_work` (LLM tool call batches cannot reference sibling call outputs)
- **Why not chosen:** More complex than needed; the Superseded state achieves the same
  semantic distinction with less machinery

### Alternative 3: Simple `all(Done)` Without Superseded

- **Description:** Just change GoalComplete to require `all(Done)`, treating Abandoned as a
  blocker
- **Pros:** Minimal change
- **Cons:** Introduces a deadlock: when the coordinator legitimately abandons a work and
  creates a replacement, the original Abandoned work remains in the taskstore permanently.
  GoalComplete can never fire because the old item is not Done and never will be.
- **Why not chosen:** Structurally unsafe

## Technical Considerations

### Dependencies

- `FlexibleEnum` derive macro (`loopr-derive`) generates `FromStr` and `VARIANT_NAMES` from
  the Rust enum variants at compile time - it does NOT read FSM YAML. The FSM YAML is loaded
  at runtime by `FsmInterpreter`. Adding `Superseded` requires updating both: the Rust enum
  AND the YAML (they are independent but must stay in sync).
- All exhaustive `match` statements on `WorkStatus` and `HierarchyStatus` will produce
  compiler errors until updated - use this as a mechanical checklist for all call sites.

**Two categories of terminal checks - treat them differently:**

When fixing compiler errors, distinguish between:

1. **"Is this item terminal?" (safe to ignore / stop tracking)** - add `Superseded` alongside
   `Done | Abandoned`. These sites are correct to include Superseded:
   - `coordinator.rs:61,134,272,298,324,971` - filtering active items for context/scheduling
   - `coordinator/run.rs:463` - checking if work is still active
   - `fsm/status.rs` terminal list

2. **"Is this item successfully complete?" (GoalComplete / phase completion gates)** - must
   NOT include `Abandoned`; `Superseded` is transparent (ignored):
   - `reconcile.rs:309,321,450,465` - all completion gates
   - `generation.rs:274` - `is_phase_complete` function - **needs the same fix as the reconciler**
   - `primitive/catalog/reconcile.rs:44-45` - summary counters (add `Superseded` counter)

### Performance

No performance impact. All changes are in the reconciler's O(N) passes over the in-memory
store maps, which are already fast.

### Security

No security implications.

### Testing Strategy

- FSM transition tests: `Superseded` reachable from each pre-terminal Work/Hierarchy state by
  coordinator; no transitions out of `Superseded`; non-coordinator roles cannot transition to
  `Superseded`
- Reconciler unit tests:
  - GoalComplete false when any Work is Abandoned (Brief mode)
  - GoalComplete false when any Spec is Abandoned or Active (Full mode)
  - GoalComplete true when all Works Done (Brief), all Specs Complete (Full)
  - GoalComplete true when mix of Done + Superseded (no Abandoned)
  - Phase does NOT complete when any child is Abandoned (stays Active)
  - Phase does NOT complete when all children are Superseded and none are Done
  - Phase DOES complete when mix of Done + Superseded with at least one Done
- E2E: re-run `python-api` target; confirm GoalComplete does not fire when majority of works
  are Abandoned; confirm coordinator uses Superseded for replacement chains

### Rollout Plan

This is an additive change - `Superseded` is a new terminal state that existing coordinators
will not emit until the prompt is updated. Old JSONL data with no `Superseded` records is
unaffected. The reconciler falls back correctly to existing behavior until coordinators start
using the new state.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coordinator uses Abandoned instead of Superseded when replacing work | Medium | High | Phase hangs permanently; surfaced as stuck-Active in TUI; human must intervene and restart |
| NeedHelp signal lost or coordinator iteration budget exhausted | Low | High | Work stays Blocked, Phase stays Active, GoalComplete never fires; visible as stuck state in TUI |
| Rebase before bundle proposal fails on conflict, blocking implementer | Medium | Medium | Explicit `git rebase --abort` cleanup + descriptive error; implementer retries; existing abandon logic applies |
| All children Superseded, none Done (orphaned phase) | Low | High | `any(Done)` guard prevents Phase from completing; Phase stays Active; coordinator must use `override_phase` |
| OverridePhase/Spec misused to bypass legitimate failures | Low | Medium | Reason field is required and logged; coordinator prompt specifies Superseded vs Abandoned semantics |
| Tick publish guard masks genuine bug | Low | Low | Log skipped publish with full context; does not hide root cause |

## Resolved Questions

**Dependency re-wiring (auto vs manual):**

Leave it as a coordinator responsibility. Auto-wiring dependencies implies the system knows the
exact semantic relationship between Work A and its replacement. But the coordinator may replace
Work A with two smaller items (Work A1 and Work A2) - the daemon cannot safely guess which one
satisfies downstream Work B's dependency. The coordinator defined the new topology; the
coordinator must explicitly re-wire Work B to depend on the correct new nodes.

**Location of the max-attempts auto-abandon code path:**

`src/daemon/handlers/work.rs` lines 500-509. When the coordinator transitions a work back to
`Ready`, the daemon intercepts this and silently overrides the target status to `Abandoned` if
`attempt_count >= MAX_WORK_ATTEMPTS`:

```rust
// src/daemon/handlers/work.rs:500-509 (BEFORE - current code)
let effective_status = if target_status == WorkStatus::Ready && from != WorkStatus::Draft {
    wi.attempt_count += 1;
    if wi.attempt_count >= MAX_WORK_ATTEMPTS {
        WorkStatus::Abandoned       // <-- bypasses coordinator
    } else {
        target_status
    }
} else {
    target_status
};
```

Phase 5 changes this to `WorkStatus::Blocked` and emits a NeedHelp learning/event immediately
after persistence (around line 512) so the coordinator can evaluate and call either `Superseded`
or `Abandoned`.

## Open Questions

None remaining. All questions resolved during Architect review.

## References

- `src/agents/coordinator/reconcile.rs` - reconciliation logic
- `src/domain/work.rs` - WorkStatus enum
- `src/domain/plan.rs` - HierarchyStatus enum
- `src/fsm/status.rs` - FsmStatus trait implementations
- `strategies/fsm/work.yml` - work FSM YAML definition
- `strategies/fsm/hierarchy.yml` - hierarchy FSM YAML definition
- `src/agents/integrator.rs` - tick lifecycle and merge failure handling
- `src/agents/executor/util.rs` - `resolve_worktree_base_for`
- `src/worktree/manager.rs` - `create_branch` / `get_or_create_branch`
