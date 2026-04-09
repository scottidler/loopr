# Design Document: E2E Stability - Reviewer Spawning, Integrator Conflict Recovery, and Decomposer File Awareness

**Author:** Scott A. Idler
**Date:** 2026-04-09
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Three cascading failures killed a python-api e2e run in ~11 minutes: the decomposer created 4 parallel works all targeting `database.py`, the integrator hit a merge conflict and abandoned works without recovery, and two remaining bundles sat Triaged forever because the reviewer spawn hook silently swallowed pool_exhausted errors. This design addresses all three failure modes plus supporting fixes.

## Problem Statement

### Background

During a python-api e2e run on v0.1.111, the orchestrator decomposed a Plan into 2 Specs, 2 Phases, and 9 Work items. The first Phase had 4 Works targeting `database.py` (get_db_path+init_db+get_connection, get_bookmark+delete_bookmark, create_bookmark+list_bookmarks, update_bookmark) plus a test suite Work depending on all four.

### Problem

**Failure chain:**

1. **Decomposer** created 4 parallel Works all writing to `database.py` with no dependencies. The decomposer prompt (`prompts/decompose/work.pmt:47-49`) explicitly instructs the LLM: *"If multiple Work items write to the same file, that is fine. The Integrator handles merge conflicts at integration time."* This is wrong - the integrator cannot handle this.

2. **Integrator** merged `agent/wk-xbt81` into `integration/pl-9qn2i`, then tried `agent/wk-mcmk2` which conflicted on `database.py`. It abandoned BOTH works, but `wk-xbt81`'s merge was already committed - git/taskstore state inconsistency. The integrator's only recovery is to abandon works and create a Learning, hoping the coordinator will redecompose - but the coordinator can't formulate the `override_work` + `create_work` payload.

3. **Reviewer spawning** is push-based: a one-shot `agent.start` fires on `bundle.transition` to Triaged (`src/daemon/handlers.rs:281-297`). With `max_pool: 2` for reviewers (`src/config.rs:456`), only 2 of 4 bundles got reviewers. The 3rd and 4th hit `pool_exhausted` and the error was silently dropped: `let _ = Box::pin(dispatch(...)).await;` (`src/daemon/handlers.rs:296`). The reconciler (`src/daemon/context.rs`) only sweeps `Integrating` bundles, not `Triaged`. Result: 2 bundles dead forever, 32 workers idle.

4. **Coordinator context** (`src/agents/coordinator.rs:877-936`) shows work IDs and statuses but NOT bundle IDs. The coordinator saw works as InReview, tried `accept_bundle("wk-yfr4k")` instead of `accept_bundle("bd-3guf7")`, hit the same error 3 times, and the lifeguard killed it.

### Goals

- Triaged bundles must always get reviewers, even under pool pressure
- Merge conflicts on overlapping files must recover by combining works, not abandoning
- Decomposer must strongly prefer keeping same-file work in one Work item
- Coordinator must see bundle IDs in its context
- `touched_paths` renamed to `paths` (readability)

### Non-Goals

- Changing the decomposer to hard-ban same-file parallel works (strong discouragement, not prohibition)
- Fixing the coordinator's `override_work` + `create_work` prompt (the integrator combining fix eliminates the need for coordinator-driven recovery)

## Proposed Solution

### Overview

Seven changes across four subsystems. Phases 1-5 are mechanical/Sonnet work. Phases 6-7 are architectural/Opus work.

| Phase | Change | Subsystem | Model |
|-------|--------|-----------|-------|
| 1 | Rename `touched_paths` -> `paths` | domain | Sonnet |
| 2 | Reconciler sweeps Triaged bundles | daemon | Sonnet |
| 3 | Log reviewer spawn failures (fix `let _ =`) | daemon | Sonnet |
| 4 | Coordinator context includes bundle IDs | coordinator | Sonnet |
| 5 | Decomposer discourages same-file splits | decomposer | Sonnet |
| 6 | Integrator combines conflicting works | integrator | Opus |
| 7 | Worker pool unification: reviewers become pull-based | daemon, worker, config | Opus |

### Phase 1: Rename `touched_paths` -> `paths`

**Files:** `src/domain/bundle.rs` and all references across ~15 source files.

Mechanical rename. The Bundle struct field `touched_paths: Vec<String>` becomes `paths: Vec<String>`. All source file references updated. Test JSON strings updated. Doc files left as-is (historical).

No `serde(rename)` or backward compatibility. Clean break.

### Phase 2: Reconciler sweeps Triaged bundles

**File:** `src/daemon/context.rs` (reconcile_state function, currently lines 786-809)

Add a new sweep block after the existing `Integrating -> Accepted` sweep:

```
For each Triaged bundle:
  Check if any non-terminal reviewer session exists for this bundle_id
  If none: log a warning and dispatch agent.start for a reviewer
```

This is the quick fix that prevents the deadlock. If `agent.start` fails again (pool still full), the next reconciliation sweep (every 60s) retries.

**Dedup guard:** Before dispatching `agent.start`, check that no non-terminal reviewer session already exists for this `bundle_id`. This prevents double-spawning if a reviewer is starting but hasn't persisted its session yet.

As a Phase 2 stopgap for `max_pool`, set reviewer `max_pool` to `MAX_POOL_UNLIMITED` so it resolves to `worker_pool_size`. Phase 7 removes `max_pool` from reviewer config entirely.

### Phase 3: Log reviewer spawn failures

**File:** `src/daemon/handlers.rs` (line 296)

Change `let _ = Box::pin(dispatch(...)).await;` to log the error:

```rust
let resp = Box::pin(dispatch(...)).await;
if resp.is_error() {
    tracing::warn!(
        "[auto-start] failed to spawn reviewer for bundle {}: {:?}",
        bid, resp.error
    );
}
```

This makes pool_exhausted errors visible instead of silently dropped.

### Phase 4: Coordinator context includes bundle IDs

**File:** `src/agents/coordinator.rs` (append_work_status function, lines 877-936)

When rendering actionable works with status InReview, look up the associated bundle from the bundles store and append the bundle ID:

```
- [wk-yfr4k] update_bookmark (InReview) [bundle: bd-3guf7, Triaged]
```

This gives the coordinator the `bd-*` ID it needs for `accept_bundle` calls. Read bundles store, filter by `work_id`, take the latest non-terminal bundle.

### Phase 5: Decomposer discourages same-file splits

**File:** `prompts/decompose/work.pmt` (lines 36-49, the Parallelism section)

Current text (lines 47-49):
```
- If multiple Work items write to the same file, that is fine. The Integrator
  handles merge conflicts at integration time. Do NOT add dependencies
  between Work items solely because they touch the same file.
```

Replace with:
```
- STRONGLY AVOID splitting work on the same file across parallel Work items.
  If multiple functions, classes, or sections target the same source file,
  combine them into a single Work item. Same-file parallel writes cause merge
  conflicts that waste cycles. Only split same-file work if the items are
  genuinely too large for one implementer (>500 lines of new code each).
  When you must split same-file work, add explicit dependencies so they
  execute sequentially, never in parallel.
```

### Phase 6: Integrator combines conflicting works

**File:** `src/agents/integrator.rs`

Current behavior (`escalate_structural_conflict`, lines 1151-1232):
1. Abandon all conflicting works
2. Create a Learning telling the coordinator to redecompose
3. Hope the coordinator figures it out (it doesn't)

New behavior - `combine_conflicting_works`:

```
1. Attempt git merge (always)
2. Merge succeeds? Done. Continue to next bundle.
3. Merge fails:
   a. Abort the merge, reset integration branch to pre-tick HEAD
   b. Call classify_conflict(paths) on all bundles in the tick
   c. If NO paths overlap: reset conflicting works to Ready (retry next tick)
   d. If paths overlap detected:
      i.   Read all conflicting Work items (titles, descriptions, acceptance_criteria, parent_id)
      ii.  Create ONE new Work (handles 2+ conflicting works, not just pairs):
           - title: "{Work A title} + {Work B title} [+ ...]"
           - description: concatenation of all descriptions
           - acceptance_criteria: union of all AC lists (cap at 20; if exceeded, summarize)
           - parent_id: same Phase as originals
           - dependencies: union of all originals' deps, MINUS the IDs being combined
             (prevents self-referential cycles)
           - status: Ready
      iii. For each Work in the same Phase that depends on either original:
           replace the dependency ID with the new combined Work ID
      iv.  Abandon all original conflicting Works (reason: "combined into {new_work_id}")
      v.   Reject all original conflicting Bundles
      vi.  Create Learning: "STRUCTURAL CONFLICT RESOLVED: {work_a} + {work_b}
           combined into {new_work_id} due to overlapping paths: {files}"
```

The combined Work dispatches to a fresh implementer who writes the file coherently in one pass. No coordinator involvement needed.

**Key details:**
- The new Work must inherit all dependencies from both originals (union), and any Works in the same Phase that depended on either original must have their dependency updated to point to the new combined Work. Cross-phase dependencies don't exist (decomposer rule), so only same-Phase works need rewiring.
- The integrator must reset the integration branch to its pre-tick state before combining. The e2e run showed `wk-xbt81` stayed merged even after abandonment - the tick's partial merges must be rolled back before any recovery action.

### Phase 7: Worker pool unification (reviewers become pull-based)

Phase 2 is the stopgap. Phase 7 is the architectural fix that eliminates the fragile push-based reviewer spawning entirely.

**Goal:** Workers pull both Ready works (for implementers) AND Triaged bundles (for reviewers) from a unified queue. No more one-shot `agent.start` hooks for reviewers. No more `max_pool` per agent type.

**Step 1: Extend `next_assignable_work` to return review assignments**

**File:** `src/daemon/work_queue.rs`

Currently `next_assignable_work` only finds Ready Works and claims them for implementers. Create a new function `next_assignment` that returns an enum:

```rust
pub enum Assignment {
    Implement(String),  // work_id
    Review(String),     // bundle_id
}
```

`next_assignment` checks two pools in priority order:
1. Triaged bundles with no active (non-terminal) reviewer session -> `Assignment::Review(bundle_id)`
2. Ready works with no active implementer session -> `Assignment::Implement(work_id)`

Review assignments take priority because reviewers are short-lived (~7s) and unblock the integration pipeline.

**Step 2: Update worker loop**

**File:** `src/agents/worker.rs`

The `run_worker` function currently calls `next_assignable_work` and runs `run_single_work`. Change it to call `next_assignment` and dispatch based on the enum:

```
match next_assignment(&stores) {
    Some(Assignment::Implement(work_id)) => run_single_work(...)
    Some(Assignment::Review(bundle_id)) => run_single_review(...)
    None => idle
}
```

`run_single_review` is extracted from the existing reviewer agent start path in `src/daemon/handlers/agent.rs` - same session creation, same executor, just pulled instead of pushed.

**Step 3: Delete the push-based reviewer hook**

**File:** `src/daemon/handlers.rs` (lines 281-297)

Delete the entire `if method == "bundle.transition" && target == Triaged` block that fires `agent.start` for reviewers. This is the `let _ = Box::pin(dispatch(...)).await;` code. Gone completely.

The auto-triage hook (lines 261-279, `bundle.create` -> `bundle.transition` to Triaged) stays - bundles still need to transition to Triaged. But spawning the reviewer is now the worker pool's job.

**Step 4: Remove `max_pool` from reviewer config**

**File:** `src/config.rs`

Remove `max_pool` from `AgentRoleConfig` entirely. All concurrency is governed by `worker_pool_size`. Workers naturally self-limit: if all 32 workers are busy (implementing or reviewing), no more work is pulled until one finishes.

If other agent types still need `max_pool` (coordinator, integrator are singletons), keep it for those but remove it from implementer and reviewer configs since workers handle their concurrency.

**Step 5: Remove reviewer `assign_agent` from coordinator prompt**

**File:** `prompts/coordinator.pmt` (line 28)

Current text:
```
`assign_agent` ... for reviewers: {"action": "assign_agent", "agent_type": "reviewer", "target_id": "bundle-id"}
```

Remove the reviewer clause. The coordinator should never manually assign reviewers - workers handle it automatically. Keep the implementer override clause (it's a rare escape hatch).

Also remove the `InReview` mention at line 51:
```
- "InReview": Force to review if a valid Bundle exists
```

The coordinator doesn't force reviews anymore. The worker pool handles it.

### Data Model

Bundle struct change (Phase 1):
```rust
// Before
pub touched_paths: Vec<String>,

// After
pub paths: Vec<String>,
```

No new types or tables.

### Implementation Plan

Phases are ordered to group Sonnet work (mechanical, low-risk) before Opus work (architectural, medium-risk):

1. **Phase 1** (rename) - zero risk, mechanical, unblocks Phase 6's `paths` references
2. **Phase 2** (reconciler) - low risk, fixes the immediate deadlock
3. **Phase 3** (logging) - low risk, makes spawn failures visible
4. **Phase 4** (coordinator context) - low risk, additive change to string building
5. **Phase 5** (decomposer prompt) - zero code risk, prompt text change
6. **Phase 6** (integrator combining) - medium risk, new code path in conflict recovery
7. **Phase 7** (worker pool unification) - medium risk, replaces push-based reviewer spawning with pull-based. Depends on Phase 2 being proven stable first. Deletes the code Phase 3 patches.

## Alternatives Considered

### Alternative 1: Pre-flight conflict detection (skip merge on path overlap)

- **Description:** Before attempting git merge, compare `paths` across bundles and skip directly to combining if overlap exists.
- **Pros:** No wasted git operations.
- **Cons:** Sometimes git CAN merge same-file changes cleanly (e.g., changes to different sections). Skipping the merge is overly conservative.
- **Why not chosen:** User preference: always attempt the merge first. Only combine after actual failure + path overlap confirmed.

### Alternative 2: Coordinator-driven recovery

- **Description:** Keep the current Learning-based approach where the integrator abandons and the coordinator redecomposes.
- **Pros:** No new integrator logic needed.
- **Cons:** The coordinator LLM can't reliably formulate `override_work` + `create_work` payloads (observed bug). This path is fundamentally broken.
- **Why not chosen:** Proven failure mode. Mechanical combining in the integrator is more reliable than LLM-driven recovery.

## Technical Considerations

### Dependencies

- Phase 1 (rename) must complete before Phase 6 (integrator uses the `paths` field)
- Phases 2, 3, 4, 5 are independent of each other
- Phase 7 depends on Phase 2 being proven stable (Phase 2 is the stopgap; Phase 7 replaces and deletes its patched code path)

### Testing Strategy

- **Phase 1:** `otto ci` after rename - compiler catches any missed references
- **Phase 2:** Unit test: mock a Triaged bundle with no reviewer session, verify reconciler dispatches agent.start. Integration test: verify retry on next sweep if first attempt fails.
- **Phase 3:** Unit test: verify warn log emitted when dispatch returns error. Verify no `let _ =` remains on the reviewer spawn path.
- **Phase 4:** Unit test: verify bundle ID appears in coordinator context string when work is InReview
- **Phase 5:** E2e observation: verify decomposer produces fewer same-file parallel works on python-api target
- **Phase 6:** Unit test: two bundles with overlapping paths, verify combined Work is created with merged AC. Integration test: e2e run where same-file conflict triggers combining.
- **Phase 7:** Unit test: `next_assignment` returns `Review(bundle_id)` when Triaged bundle exists with no active reviewer. Unit test: `next_assignment` returns `Implement(work_id)` when no Triaged bundles exist. Integration test: full e2e run with 4+ simultaneous bundles, all get reviewed without deadlock. Verify push-based hook is deleted (grep for the old code path, assert absent).

### Rollout Plan

Ship all phases in a single branch. Run `otto ci` after each phase. Run `/e2e python-api` after all phases to validate end-to-end.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Combined Work AC list too large for implementer context | Low | Med | Cap combined AC at 20 items; if exceeded, split sequentially |
| Reconciler retry storm if reviewer pool stays full | Med | Low | Reconciler runs every 60s with logged warnings; pool exhaustion is visible |
| Decomposer prompt change causes over-combining (giant single Works) | Low | Med | Prompt says "only split if >500 lines each" as escape valve |
| Dependency rewiring in Phase 6 misses a dependent Work | Med | High | Query all Works in Phase, update any that reference either original |
| Phase 7 worker starvation: reviews monopolize all workers | Low | Med | Reviews are ~7s; implementer work is minutes. Natural backpressure: reviews drain fast, workers return to implementing |
| Phase 7 removes Phase 2 reconciler sweep prematurely | Low | High | Keep the reconciler sweep as defense-in-depth even after Phase 7. Belt and suspenders. |

## Open Questions

- [x] Should the integrator revert the already-merged branch (wk-xbt81) before combining? **Yes.** The tick's partial merges must be rolled back. The combined Work starts fresh.
- [x] What is the right `max_pool` for reviewers? **Eliminated in Phase 7.** As a Phase 2 stopgap, set to `MAX_POOL_UNLIMITED`. Phase 7 removes `max_pool` from reviewer config entirely.

## References

- E2e run telemetry: python-api 2026-04-09T21:15-21:28
- `src/daemon/handlers.rs:281-297` - reviewer spawn hook
- `src/daemon/handlers/agent.rs:111-138` - pool enforcement
- `src/daemon/context.rs:786-809` - reconciler Integrating sweep
- `src/config.rs:450-461` - default_reviewer max_pool: 2
- `src/agents/integrator.rs:1151-1232` - escalate_structural_conflict
- `src/agents/coordinator.rs:877-936` - append_work_status (missing bundle IDs)
- `prompts/decompose/work.pmt:47-49` - "same file is fine" instruction
- `src/daemon/work_queue.rs:32-98` - next_assignable_work (implementer-only pull)
- `src/agents/worker.rs:26-78` - run_worker loop (implementer-only dispatch)
- `prompts/coordinator.pmt:28` - assign_agent reviewer clause (to be removed)
