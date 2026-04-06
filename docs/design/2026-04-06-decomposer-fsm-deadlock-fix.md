# Design Document: Decomposer FSM Deadlock Fix

**Author:** Scott Idler
**Date:** 2026-04-06
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

When decomposition fails, the coordinator FSM deadlocks in `Decomposing` forever because
the failure is never persisted to the TaskStore and the FSM has no handler for it. Two
additional gaps compound the problem: a missing struct field makes the existing failure-check
code unreachable, and completed spec branches are silently discarded when any one branch fails.
This document fixes all three.

## Problem Statement

### Background

The decomposer runs as a background `tokio::spawn` task in `doc.rs`. It calls
`decompose_hierarchy`, which decomposes a plan into specs, phases, and works. On success it
persists everything and broadcasts `decomposition.completed`. On failure it broadcasts
`decomposition.failed`. The coordinator subscribes to these events to wake up.

### The Three Bugs

**Bug 1: `decomposition_error` field missing from struct**

`run.rs:184` already checks `coord_state.decomposition_error` and returns `NeedsHelp` if set.
`doc.rs:320` already writes to `coord_state.decomposition_error` on `decompose_hierarchy`
failure. But the field was never added to the `CoordinatorState` struct definition. The code
compiles because `decomposition_error: None` appears in `CoordinatorState::new()` - but the
struct itself has no such field, so the field initializer is silently matching something else
or this is a latent compile error masked by the existing build artifact. Either way, the
check in `run.rs:184` never fires.

**Bug 2: persist-failed path does not write `decomposition_error`**

There are two failure paths in the background task:
- `decompose_hierarchy` returns `Err` - this path writes `decomposition_error` (lines 319-322)
- `double_write_old_records` returns `Err` (persist failed after successful decomposition) -
  this path broadcasts `decomposition.failed` but does NOT write `decomposition_error`

If the persist fails, the coordinator is woken up by the event but on the next iteration reads
`decomposition_error: None` and goes back to idle. Same deadlock.

**Bug 3: try_join_all discards completed spec branches on partial failure**

`decompose_hierarchy` runs spec branches with `try_join_all(spec_futures).await?`. In the
python-api run, two of three spec branches (API Routes: 12 docs, Test Suite: 11 docs) completed
successfully before Database Layer failed. `try_join_all` propagated the error and the calling
code returned `Err` without touching the completed results. All 23 docs were discarded. The
persist block was never reached. Zero docs in the TaskStore.

### Goals

- `decomposition_error` is a real struct field on `CoordinatorState`
- Both failure paths in `doc.rs` write `decomposition_error` before broadcasting the event
- The coordinator exits `Decomposing` with `NeedsHelp` when `decomposition_error` is set,
  regardless of whether the event was received
- Completed spec branches are persisted even when a sibling branch fails
- The existing FSM check in `run.rs:184` fires correctly (it is already written correctly)

### Non-Goals

- Automatic retry of failed decomposition (the coordinator surfaces `NeedsHelp`; retry is a
  separate concern)
- Changing the decomposer's retry logic for individual LLM parse failures
- Any changes to the coordinator's happy path

---

## Proposed Solution

### Overview

Three surgical changes:

1. Add `decomposition_error: Option<String>` to `CoordinatorState` struct
2. Add the missing `decomposition_error` write to the persist-failed path in `doc.rs`
3. Replace `try_join_all` in `decompose_hierarchy` with `join_all` + error collection so
   completed branches are persisted even when one branch fails

### Data Model Change

Add to `CoordinatorState` in `src/domain/coordinator_state.rs`, between `phase_activated_at`
and `decomposition_attempts`:

```rust
pub phase_activated_at: Option<i64>,
/// Error message from the most recent decomposition failure. When set, the coordinator
/// transitions to NeedsHelp instead of busy-polling. Cleared on re-decompose.
#[serde(default)]
pub decomposition_error: Option<String>,
/// Decomposition attempt count per parent ID.
#[serde(default)]
pub decomposition_attempts: HashMap<String, u32>,
```

This field is already referenced at `run.rs:184` and `doc.rs:320` and initialized to `None`
in `CoordinatorState::new()` at line 106. Adding the struct field is the only change needed
to make those paths live.

### `doc.rs` - persist-failed path

The `double_write_old_records` failure path (around line 302) currently only broadcasts the
event. Add the durable write:

```rust
Err(e) => {
    // existing log ...
    // NEW: persist error durably before broadcasting
    if let Ok(mut cs) = stores_bg.read_coordinator_state(&goal_id_bg) {
        cs.decomposition_error = Some(e.to_string());
        let _ = stores_bg.write_coordinator_state(&cs);
    }
    let _ = event_tx_bg.send(DaemonEvent::new(
        "decomposition.failed",
        json!({ "goal_id": goal_id_bg, "error": e.to_string() }),
    ));
}
```

The `decompose_hierarchy` failure path (around line 314) already does this correctly.

### `decomposer.rs` - partial branch persistence

Replace the `try_join_all` pattern with `join_all` and explicit error collection. Completed
branches are persisted; failed branches set `decomposition_error`.

Current code:
```rust
let branch_results = try_join_all(spec_futures).await?;
```

Replacement:
```rust
let branch_results_raw = join_all(spec_futures).await;

let mut branch_results = Vec::new();
let mut branch_error: Option<String> = None;

for (spec, result) in specs.iter().zip(branch_results_raw) {
    match result {
        Ok(branch) => branch_results.push(branch),
        Err(e) => {
            warn!("spec branch {} '{}' failed: {}", spec.id, spec.title, e);
            branch_error = Some(format!("spec '{}': {}", spec.title, e));
        }
    }
}
```

After collecting results, proceed to merge and persist all successful branches. Then, after
the persist succeeds:

```rust
if let Some(err) = branch_error {
    // partial success: some branches failed - persist completed work, signal failure
    let _ = event_tx.send(DaemonEvent::new(
        "decomposition.failed",
        json!({ "goal_id": goal_id, "error": err }),
    ));
    return Err(eyre::eyre!(err));
}
```

This requires `decompose_hierarchy` to receive `goal_id` and `event_tx` - OR the partial
failure signal can be returned as a structured result type and handled in `doc.rs`. See
alternatives.

**Return type:** Change `decompose_hierarchy` to return `Result<(Vec<Doc>, Option<String>)>`
where the `Option<String>` carries a partial failure message when one or more spec branches
failed but at least some docs were produced.

```rust
pub async fn decompose_hierarchy<H: HttpClient + Sync>(
    plan: &Doc,
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &H,
    brief: bool,
) -> Result<(Vec<Doc>, Option<String>)>
//          ^^^^^^^^^  ^^^^^^^^^^^^^^
//          all docs   Some(err) if any spec branch failed, None if all succeeded
```

`ratify_hierarchy` is skipped when `partial_err` is `Some` - ratification requires a complete
hierarchy. The partial docs are still persisted so the coordinator has something to work with.

`doc.rs` updated to:
```rust
match decompose_hierarchy(...).await {
    Ok((child_docs, partial_err)) => {
        // persist all docs regardless of partial failure
        match double_write_old_records(..., &child_docs, ...) {
            Ok(()) => {
                for child in child_docs { persist_doc(...); }
                if let Some(err) = partial_err {
                    // write decomposition_error, broadcast decomposition.failed
                } else {
                    // broadcast decomposition.completed
                }
            }
            Err(e) => {
                // write decomposition_error, broadcast decomposition.failed
            }
        }
    }
    Err(e) => {
        // total failure (e.g. plan doc unreadable, plan has zero specs)
        // existing path - write decomposition_error, broadcast decomposition.failed
    }
}
```

### `run.rs` - no change needed

The check at lines 183-190 is already correct:

```rust
if coord_state.fsm_state == CoordinatorFsmState::Decomposing {
    if let Some(err) = &coord_state.decomposition_error {
        return Ok(IterationOutcome::NeedHelp(
            format!("Background decomposition failed: {}", err)
        ));
    }
    return Ok(IterationOutcome::Done("waiting for decomposition to complete".to_string()));
}
```

Once Bug 1 is fixed, this fires on every coordinator iteration after a failure, regardless
of whether the event was received.

### Pre-existing compile error

`doc.rs:970-972` contains corrupt text (`ator must be running");`) outside any function.
Remove these lines as part of this work.

### Implementation Plan

**Phase 1 - Add `decomposition_error` field to `CoordinatorState`**
- Add the field to the struct definition in `coordinator_state.rs`
- Confirm `decomposition_error: None` in `new()` (already present at line 106)
- Add `decomposition_error: null` to any hardcoded JSON fixtures in tests that construct
  coordinator state (the `#[serde(default)]` handles JSONL deserialization automatically)
- Run `otto ci` - must compile clean

**Phase 2 - Fix persist-failed path in `doc.rs`**
- Add `decomposition_error` write to the `double_write_old_records` failure arm
- Remove corrupt lines 970-972
- Run `otto ci`

**Phase 3 - Partial branch persistence in `decomposer.rs`**
- Change `decompose_hierarchy` return type to `Result<(Vec<Doc>, Option<String>)>`
- Replace `try_join_all(spec_futures).await?` with `join_all` + per-branch error collection
- Update `doc.rs` to handle the new return type
- Update all test call sites in `decomposer.rs` (search for `decompose_hierarchy(`)
- Run `otto ci`

**Phase 4 - Test coverage**
- Add test: `decompose_hierarchy` with one failing spec branch returns the other branches'
  docs and a `Some(error)` partial failure message
- Add test: coordinator FSM iteration with `decomposition_error` set returns `NeedHelp`
- Add test: coordinator FSM iteration with `decomposition_error: None` and no phases
  returns `Done("waiting...")`
- Run `otto ci`

---

## Alternatives Considered

### Alternative 1: Keep `try_join_all`, accept all-or-nothing, fix only the FSM

- **Description:** Don't change the decomposer. Just fix Bug 1 (missing field) and Bug 2
  (missing write on persist-failed path). The coordinator will surface `NeedsHelp` correctly
  and the user can retry. No partial persistence.
- **Pros:** Minimal change, lower risk
- **Cons:** A 3-minute decomposition run that succeeds 2/3 of the way throws everything away
  and must restart from scratch. The python-api run produced 23 docs that were discarded.
- **Why not chosen:** The cost of discarding completed work is real. The fix is not invasive.

### Alternative 2: Persist branches eagerly inside `decompose_spec_branch`

- **Description:** Write each spec branch's docs to the TaskStore as it completes, before
  all branches finish
- **Pros:** Docs are durable as soon as produced
- **Cons:** Violates the TaskStore write-ordering protocol (partial hierarchy in the store
  while decomposition is still in flight confuses the coordinator); cross-branch dependency
  resolution happens after all branches merge, so early persistence would write unresolved deps
- **Why not chosen:** The architectural invariant is that the full decomposed hierarchy is
  committed atomically. Partial writes break coordinator startup reconciliation.

### Alternative 3: Treat `decomposition.failed` event as sufficient (no durable state)

- **Description:** Handle the event directly in the coordinator run loop without writing to
  the TaskStore
- **Pros:** No struct change, no TaskStore write
- **Cons:** Events are lossy. If the coordinator is in its 30s sleep when the event fires,
  it misses it and busy-polls forever - the exact bug we observed. Durable state is the
  correct pattern per the TaskStore invariant.
- **Why not chosen:** Same failure mode as the current bug.

---

## Technical Considerations

### Dependencies

- No new crates
- Files changed: `coordinator_state.rs`, `doc.rs`, `decomposer.rs`
- Test fixtures in `coordinator_state.rs` tests that include hardcoded JSON may need
  `"decomposition_error": null` added (covered by `#[serde(default)]` for JSONL but
  explicit JSON in test assertions needs updating)

### Testing Strategy

- All phases gated by `otto ci`
- New tests for partial failure path in decomposer (Phase 4)
- New tests for coordinator FSM `decomposition_error` handling (Phase 4)
- E2E: python-api run with an injected LLM parse failure should persist completed branches
  and surface `NeedsHelp`, not hang

### Rollout Plan

All phases ship in one PR. `#[serde(default)]` on the new field ensures backwards compatibility
with existing JSONL records that don't have `decomposition_error`.

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| All spec branches fail - zero docs, `partial_err` set | Low | Low | `Ok((vec![], Some(err)))` - persist of zero docs is a no-op, coordinator gets `decomposition_error` and surfaces `NeedsHelp` correctly |
| Cross-branch dep resolution breaks with partial docs | Med | Med | Partial docs still go through `resolve_cross_branch_deps`; unresolvable deps become `warn` (existing behavior) |
| Test fixtures with hardcoded coordinator state JSON fail to deserialize | Low | Low | `#[serde(default)]` handles missing field in JSONL; explicit JSON test fixtures updated in Phase 1 |
| `decompose_hierarchy` return type change breaks call sites | Low | Low | Only one call site in production (`doc.rs`); compiler catches all mismatches |
| Partial success + ratification: ratifier sees incomplete hierarchy | Med | Med | Skip ratification when `partial_err` is `Some` - ratify only on full success |

---

## Open Questions

- [ ] Should `decomposition_error` be cleared when a re-decompose is triggered, or should
      it be left as a historical record and a new field track retry state?
- [ ] Should the coordinator retry decomposition automatically (increment
      `decomposition_attempts`) before escalating to `NeedsHelp`, or always go straight to
      `NeedsHelp` on first failure?
- [ ] If partial persistence is implemented, should the coordinator attempt to proceed with
      the successfully decomposed specs while marking the failed spec for retry?

---

## References

- `src/domain/coordinator_state.rs` - struct definition, missing field
- `src/daemon/handlers/doc.rs` - background decomposition task, both failure paths
- `src/decomposer.rs` - `decompose_hierarchy`, `try_join_all` at line 582
- `src/agents/coordinator/run.rs` - `Decomposing` branch, existing `decomposition_error` check
- `docs/design/2026-02-25-orchestration-spine.md` - original FSM design
