# Design Document: FSM Race Defense and Goal Completion Correctness

**Author:** Scott A. Idler
**Date:** 2026-04-14
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Three classes of bugs were discovered during the python-api E2E run and subsequently
validated by the Architect. A stale-bundle race in the coordinator causes work items to be
Abandoned incorrectly. Abandoned is incorrectly treated as goal success, causing false
GoalComplete signals. E2E target scripts swallow non-zero exit codes via an unguarded bash
pipeline, masking all test failures. This document describes six targeted fixes across two
Rust source files, three bash target scripts, and the python-api scaffold.

## Problem Statement

### Background

The python-api E2E run (2026-04-13) ended with exit code 0 (GoalComplete) but produced a
broken implementation: two Abandoned work items, two broken API endpoints, zero pytest
functions, and a wrong package manager. The Gemini Architect report declared the run a
success. A manual audit found three compounding bugs in the orchestrator plus two E2E
infrastructure issues.

### Problem

1. **Coordinator race - stale bundle override:** The coordinator context builder lists every
   rejected bundle with an unconditional ACTION to reset the work item to Ready. When a
   retry cycle completes and a new bundle enters review, the old rejected bundle remains in
   context with the stale instruction. The coordinator correctly accepts the new bundle and
   simultaneously issues the stale override in the same response, triggering a third attempt
   and pushing the work item to Abandoned.

2. **False GoalComplete:** `detect_goal_complete` in `reconcile.rs` treats `Abandoned` as a
   valid terminal success state. A goal is declared complete when all work items are
   Done OR Abandoned. The `check_abandon_gate` only runs after `GoalComplete` is already
   entered and cannot prevent the false completion.

3. **E2E pipeline exit code masking:** The `verify()` functions in Docker-based target
   scripts use `docker compose run --rm test 2>&1 | /usr/bin/tail -20`. Because `set -o
   pipefail` is not set, the pipeline returns `tail`'s exit code (always 0), masking every
   non-zero exit from Docker: test failures, build errors, pytest exit 5 (no tests), and
   runtime crashes.

4. **python-api scaffold uses pip and requirements.txt:** Project convention is uv +
   pyproject.toml everywhere. The scaffold writes requirements.txt and the Dockerfile calls
   `pip install`. Agents inherit this as the package manager pattern and produce pip-based
   implementations.

### Goals

- Any race that causes a valid in-flight bundle to be invalidated by a stale coordinator
  action must be impossible at both the prompt layer and the daemon layer
- A goal with any Abandoned work item must never signal GoalComplete - it must route to
  NeedHelp
- Non-zero exit codes from Docker-based E2E validation must propagate correctly and fail
  the verification step
- The python-api scaffold must use uv + pyproject.toml, matching all other Python targets

### Non-Goals

- Changing the Abandoned semantics for dependency promotion (Abandoned unblocks hierarchy
  promotion by design - this is correct and is not changing)
- Rearchitecting the coordinator FSM states
- Fixing the generated python-api code from the last run (the target scaffold fix will
  affect future runs)

## Proposed Solution

### Overview

Six changes grouped into three coupled pairs:

- **A (Fixes 1+2):** Two-layer race defense - suppress stale override instructions in the
  context builder (prompt layer) and add a precondition guard in the daemon handler
  (enforcement layer)
- **B (Fixes 3+4):** Goal completion correctness - Abandoned is terminal but not success;
  route to NeedHelp when any work is Abandoned
- **C (Fixes 5+6):** E2E infrastructure - add `set -o pipefail` to all Docker target
  scripts and migrate python-api scaffold to uv + pyproject.toml

### Architecture

#### Fix 1 - Context builder: suppress stale rejected-bundle override

File: `src/agents/coordinator.rs` (loopr-v4) ~line 162

Current logic filters bundles by:
- `b.status() == BundleStatus::Rejected`
- `work.status() == WorkStatus::InReview`

The intent of the second condition is "this work item has not yet been reset, so the
rejected bundle is still actionable." But after a retry cycle, the work WAS reset to Ready
and a new implementer ran and submitted a new bundle. The work is now InReview again - for
the NEW bundle. The old rejected bundle bd-X still satisfies both conditions (Rejected AND
the work is InReview) even though the situation is fully resolved. The coordinator sees the
stale rejected bundle and generates a second `override_work` for work already in review.

Fix: add a third condition inside the `for b in &rejected` loop. Before emitting the ACTION
instruction, check whether any other bundle for the same work_id is in a non-terminal state.
The `bundles` HashMap is already held by the enclosing block. If a sibling active bundle
exists, the rejected bundle is already resolved and the override instruction must be
suppressed.

```rust
// Before emitting the override ACTION, verify no active bundle exists for this work:
let has_active_bundle = bundles.values().any(|b2| {
    b2.work_id == b.work_id
        && b2.id != b.id
        && !b2.status().is_terminal(&stores.fsm)
});
if !has_active_bundle {
    summary.push_str(&format!("- [{}] REJECTED ... ACTION: ...\n", ...));
}
```

The `is_terminal(&stores.fsm)` call is consistent with the loopr-v4 FSM interpreter
pattern used throughout the codebase. Per `bundle.rs` tests, terminal bundle states are
Merged, Rejected, and Superseded - non-terminal states are Proposed, Triaged, Reviewed,
Accepted, and Integrating.

#### Fix 2 - Daemon handler: precondition guard on override to Ready

File: `src/daemon/handlers/work.rs` ~line 477

The handler currently increments attempt_count and proceeds unconditionally when
`target_status == WorkStatus::Ready && from != WorkStatus::Draft`. There is no check for
an active bundle.

Fix: insert a new precondition check between the existing InReview bundle guard (line 478)
and the `attempt_count` increment block (line 480). The check fires when
`is_override && target_status == WorkStatus::Ready`. If any bundle for this work_id is in
a non-terminal state (not Rejected, Merged, or Superseded), return `precondition_failed`.

```rust
// After the InReview bundle guard, before the attempt_count block:
if is_override && target_status == WorkStatus::Ready {
    let bundles = stores.read_bundles()?;
    let has_active_bundle = bundles.values().any(|b| {
        b.work_id == wi.id && !b.status().is_terminal(fsm)
    });
    if has_active_bundle {
        return Ok(DaemonResponse::err(
            req.id,
            RpcError::precondition_failed(
                "override_work to Ready rejected: work has an active bundle in flight"
            ),
        ));
    }
}
```

Uses `is_terminal(fsm)` (the FSM interpreter parameter already available at L378) rather
than a hardcoded status match, so the guard stays correct if new terminal states are added
to the FSM config. The guard only applies to `is_override == true`; normal worker pool
Ready transitions are unaffected.

#### Fix 3 - apply_fsm_transition: gate runs before state is persisted

File: `src/agents/coordinator.rs` ~line 691

The ordering bug: `coord_state.transition_to(GoalComplete)` mutates in-memory state and
is immediately followed by `persist_coordinator_state` inside the gate's failure path.
The CLI poll reads the persisted `GoalComplete` status and exits 0 before `NeedHelp` is
processed.

Current (broken) order:
1. `coord_state.transition_to(GoalComplete)` - state is now GoalComplete in memory
2. `check_abandon_gate` - fires, returns NeedHelp
3. `persist_coordinator_state` - persists GoalComplete to DB
4. `return Some(NeedHelp)` - too late; CLI already read GoalComplete

Fixed order:
1. `check_abandon_gate` - fires while state is still Executing
2. If gate fails: `persist_coordinator_state` - persists Executing (correct), return NeedHelp
3. If gate passes: `coord_state.transition_to(GoalComplete)` - transition now safe
4. Continue with merge + deactivate + persist + Done

```rust
if new_state == CoordinatorFsmState::GoalComplete {
    // Gate runs BEFORE transition - so failure persists Executing, not GoalComplete
    if let Some(outcome) = check_abandon_gate(stores, coord_state, prefix) {
        persist_coordinator_state(stores, coord_state);
        return Some(outcome);
    }

    // Gate passed - safe to commit the GoalComplete transition
    coord_state.transition_to(CoordinatorFsmState::GoalComplete);

    // Merge integration branch...
    // Deactivate goal...
    persist_coordinator_state(stores, coord_state);
    return Some(IterationOutcome::Done("Goal complete".to_string()));
}
```

`detect_goal_complete` in `reconcile.rs` is NOT changed - `Abandoned` continues to count
as terminal for the purpose of triggering the gate check. The gate's `max_abandon_ratio`
config controls the threshold. `ReconcileOutcome` is NOT changed.

#### Fix 4 - (removed)

Superseded by Fix 3. No changes to `reconcile.rs`, `ReconcileOutcome`, or `run.rs`.
`check_abandon_gate` is the active abandonment enforcement mechanism and is now correctly
positioned before the state transition.

#### Fix 5 - E2E verify(): propagate failure to the runner exit code

Files: `bin/e2e-targets/python-api.sh`, `bin/e2e-targets/node-api.sh`,
`bin/e2e-targets/python-scraper.sh`, `bin/e2e` (loopr repo)

**Corrected diagnosis (post-Architect review):** `bin/e2e` sources target scripts via
`source "${TARGET_FILE}"` at line 141 into the main shell, which already has
`set -euo pipefail`. Adding `pipefail` to target scripts would be a no-op.

The real problem: `verify()` sets `pass=false` internally but ends with `warn "Some
verification checks failed"` - `warn` is `echo -e` and always returns 0. So `verify()`
always exits 0. And `bin/e2e` calls `verify` at line 343 without capturing its return
value - `EXIT_CODE` is set at lines 297-301 from `loopr run` alone and is never updated
by `verify`.

Two-part fix:

1. **In each target's `verify()` function**: add `return 1` when `pass=false`:
   ```bash
   if [[ "${pass}" == "true" ]]; then
       ok "All verification checks passed"
   else
       warn "Some verification checks failed"
       return 1
   fi
   ```

2. **In `bin/e2e` at line 343**: capture verify's return code:
   ```bash
   verify || EXIT_CODE=3
   ```
   Exit code 3 means "daemon succeeded but verification failed" - distinct from timeout (1)
   and NeedHelp (2).

#### Fix 6 - python-api scaffold: uv + pyproject.toml

File: `bin/e2e-targets/python-api.sh` scaffold() function (loopr repo)

Replace the `requirements.txt` approach with `uv` + `pyproject.toml`. Update:
1. `scaffold()`: write `pyproject.toml` instead of `requirements.txt`:
   ```toml
   [project]
   name = "bookmarks-api"
   version = "0.1.0"
   requires-python = ">=3.12"
   dependencies = [
       "fastapi>=0.115",
       "uvicorn[standard]>=0.32",
       "httpx>=0.28",
       "pytest>=8.3",
   ]
   ```
2. `Dockerfile`: use the official uv base image:
   ```dockerfile
   FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim
   WORKDIR /app
   COPY pyproject.toml .
   RUN uv sync
   COPY . .
   CMD ["uv", "run", "uvicorn", "main:app", "--host", "0.0.0.0", "--port", "8080"]
   ```
3. Update `docker-compose.yml` test service command from `python -m pytest` to `uv run pytest`.
4. Update `target_goal()` to state "uv + pyproject.toml (NOT pip or requirements.txt)."
5. Update `python-api.md` constraints section to match.
6. Delete the `requirements.txt` heredoc from `scaffold()`.

### Implementation Plan

#### Phase 1: FSM race defense (Fixes 1+2) - loopr-v4
**Model:** opus
- `coordinator.rs`: add `has_active_bundle` check inside the rejected-bundle loop before emitting ACTION
- `work.rs`: add precondition guard (is_override + active bundle) using `is_terminal(fsm)`, not hardcoded match
- `otto ci`

#### Phase 2: apply_fsm_transition ordering fix (Fix 3) - loopr-v4
**Model:** opus
- `coordinator.rs`: move `check_abandon_gate` call above `coord_state.transition_to(GoalComplete)`
- No changes to `reconcile.rs`, `ReconcileOutcome`, or `run.rs`
- `otto ci`

#### Phase 3: E2E infrastructure (Fixes 5+6) - loopr repo
**Model:** sonnet
- `bin/e2e-targets/python-api.sh`: `verify()` returns 1 on failure; scaffold to uv + pyproject.toml; update docker-compose.yml test command
- `bin/e2e-targets/node-api.sh`: `verify()` returns 1 on failure
- `bin/e2e-targets/python-scraper.sh`: `verify()` returns 1 on failure
- `bin/e2e` line 343: `verify || EXIT_CODE=3`
- `bin/e2e-targets/python-api.md`: update constraints
- No otto ci (bash project); manual review

#### Phase 4: Integration E2E run
**Model:** N/A (observation)
- `/e2e python-api` to validate all fixes end-to-end

## Alternatives Considered

### Alternative 1: Daemon guard only (no context builder fix)
- **Description:** Only add the precondition guard in `work.rs`. The coordinator LLM will
  still emit the stale override but the daemon will reject it.
- **Pros:** Simpler - one change instead of two
- **Cons:** The coordinator now generates one invalid action per retry cycle. This wastes
  the LLM's action budget, clutters logs, and may confuse the coordinator's understanding
  of its own state. The prompt layer should be clean.
- **Why not chosen:** The Architect agreed that both layers are warranted. Defense-in-depth
  is the right approach for a system that executes LLM-authored actions.

### Alternative 2: Filter rejected bundles by recency (show only most recent)
- **Description:** In the context builder, only show the most recently rejected bundle per
  work item, not all rejected bundles.
- **Pros:** Simple heuristic, likely correct in most cases
- **Cons:** Doesn't directly address the race condition. The most recent rejected bundle
  might still be shown with a stale override ACTION even though a new bundle is in flight.
- **Why not chosen:** Doesn't fix the root cause.

### Alternative 3: Treat Abandoned as NeedHelp at the Phase/Spec completion level
- **Description:** When complete_phases or complete_specs runs and finds any Abandoned
  child, mark the Phase/Spec as Abandoned and propagate up.
- **Pros:** Cleaner propagation - the hierarchy itself reflects abandonment
- **Cons:** Requires changes to the bottom-up completion logic and changes the meaning of
  HierarchyStatus::Abandoned. Creates a new question: what does Abandoned mean at the Spec
  level vs. the Phase level?
- **Why not chosen:** The current design already uses Abandoned at hierarchy levels for
  dependency unblocking. Conflating "some children abandoned" with "this node abandoned" is
  a semantic change that needs its own design doc. Fix 3+4 are targeted and don't require
  this.

## Technical Considerations

### Dependencies

- `loopr-v4` repo (`~/repos/scottidler/loopr-v4`): Fixes 1-4, applied to `src/`
- `loopr` repo (`~/repos/scottidler/loopr`): Fixes 5-6, applied to `bin/e2e-targets/`
- The two repos are separate checkouts. After applying Fixes 1-4 to loopr-v4, the binary
  must be compiled (`cargo build --release`) and the daemon restarted before running
  `/e2e` validation, as the e2e script uses `loopr/target/release/loopr`.

### Testing Strategy

- Fix 1: Unit test that context builder omits ACTION for a work item with an active bundle.
  Test that it still shows ACTION when there is no active bundle.
- Fix 2: FSM test that override_work to Ready fails with precondition_failed when an active
  bundle exists.
- Fix 3+4: Unit tests for `detect_goal_complete` and `detect_goal_abandoned`:
  - all Done -> goal_complete=true, need_help=None
  - all Done + one Abandoned -> goal_complete=false, need_help=Some(...)
  - mixed in-progress -> goal_complete=false, need_help=None
- Fixes 5+6: E2E run with `/e2e python-api`

### Rollout Plan

Apply changes directly to the v4 branch. No feature flag needed - all fixes are
correctness improvements to existing behavior, not new features.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Goals with any abandoned work never complete (all-Done requirement is too strict) | Low | High | This is the correct behavior: any abandonment = NeedHelp, operator intervenes |
| detect_goal_abandoned fires when ALL works abandoned (none Done) - valid but verbose reason | Low | Low | Covered: Brief mode checks any-Abandoned, which fires in this case too |
| Daemon precondition guard blocks a legitimate override when bundle IS terminal | Low | Medium | Guard only fires for non-terminal bundles; a Rejected/Merged/Superseded bundle does not trigger it |
| uv Docker image not available in air-gapped environments | Very Low | Low | Target is for CI/E2E use only; no air-gap concern |
| pipefail breaks verify() intentional || true patterns | Low | Low | verify() already uses explicit || true on docker compose down; pipefail only affects unguarded pipes |

## Decisions Made

- **Fix 3+4 approach (post-Architect):** Rather than removing `Abandoned` from
  `detect_goal_complete` and adding zero-tolerance abandonment, fix the ordering bug in
  `apply_fsm_transition` so `check_abandon_gate` fires before GoalComplete is persisted.
  `max_abandon_ratio` is preserved as a configurable threshold.
- **check_abandon_gate:** Remains in place as the active abandonment enforcement mechanism.
  No longer dead code - it is now correctly positioned.
- **Fix 2 implementation:** Use `is_terminal(fsm)` (FSM interpreter) not hardcoded status
  match, per Architect finding that hardcoded match is a structural regression in loopr-v4.
- **Fix 5 root cause:** `verify()` always returns 0; `bin/e2e` never checks verify's exit.
  Fix is `return 1` in verify() + `verify || EXIT_CODE=3` in bin/e2e. Not pipefail.
- **python-api.md:** Constraints section updated as part of Fix 6 implementation.

## References

- `docs/2026-04-13-python-api-e2e-report-gemini.md` - Gemini's E2E run analysis (contains
  the three false claims that surfaced these bugs)
- `src/agents/coordinator.rs:162-191` - rejected bundle context section
- `src/agents/coordinator/reconcile.rs:296-323` - detect_goal_complete
- `src/agents/coordinator/run.rs:178-195` - reconcile outcome handling
- `src/daemon/handlers/work.rs:474-487` - attempt_count / override path
- Architect consultation session (2026-04-14) - validated all six fixes
