# Design Document: Decomposer Unblock

**Author:** Scott Idler
**Date:** 2026-04-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The Loopr E2E test for `python-api` has timed out on every run because `doc.inject` synchronously blocks the IPC connection for the full duration of LLM decomposition (~9 minutes of a 20-minute budget), leaving insufficient time for implementation. Three compounding bugs make the problem worse: work items are created with `title="Untitled Plan"` due to a missing `title` field on `Doc`, cross-spec dependency references are silently dropped, and the IPC caller hangs with no feedback. This document specifies fixes in priority order: title data loss, async decomposition, and cross-scope dependency resolution.

## Problem Statement

### Background

`loopr run` sends a `doc.inject` IPC call to the daemon, which calls `accept_plan_markdown`. This function runs the full `Plan -> Spec -> Phase -> Work` decomposition synchronously before creating the `CoordinatorGoal` or returning. With 3 specs, 8 phases, and ~20 works each requiring an LLM call (~25 s average), the decomposition wall-clock time is ~9 minutes. The 20-minute E2E timeout then leaves only ~11 minutes for all implementation work.

Measured in the 2026-04-05 python-api run:
- Decomposition started: 16:21:59
- Decomposition completed: 16:30:47
- Elapsed: 8 min 48 sec
- First work assigned: 16:30:48 (one second after decomposition finished)
- Run killed: 16:30:53

### Problem

Three distinct bugs were observed:

**Bug 1 - Work title data loss.** `Doc` has no `title` field. `double_write_old_records` re-extracts the title from doc content using `extract_plan_title`, which scans for a `# H1` heading. Work documents use `## H2` sections (`## Parent`, `## Description`, etc.) and have no H1. Every work lands in the `works` collection with `title = "Untitled Plan"`. The coordinator receives anonymous work items; implementers lose their task title from context.

**Bug 2 - Synchronous IPC block.** `accept_plan_markdown` awaits `decompose_hierarchy` before creating `CoordinatorGoal`, `CoordinatorState`, or returning. The IPC caller (`loopr run`) hangs for 9+ minutes with no feedback. The coordinator agent sits in a starvation loop (`No active goal, waiting for goal to be set`) for the entire decomposition window.

**Bug 3 - Cross-scope dependency resolution silently dropped.** Inside `decompose_into`, dependencies are resolved against `title_to_id`, a map built only from siblings within the same `decompose_into` call (same phase). A work item in Spec A that declares a dependency on a work item in Spec B will log `dependency '{}' not found among siblings` and have its dependency silently dropped (`dependencies: []`). The coordinator has no knowledge of the required ordering.

### Goals

- Work items in the `works` collection carry their correct title from the decomposer's LLM output.
- `doc.inject` and `doc.accept` return to the caller immediately after creating the Plan Doc and CoordinatorGoal; decomposition runs in the background.
- The coordinator does not advance past `Decomposing` until decomposition is complete and the full doc hierarchy is persisted.
- Cross-spec dependency references are resolved against the full plan scope, not just the local sibling array.

### Non-Goals

- Parallel (concurrent) spec/phase decomposition - sequential order is preserved; only the IPC blocking is removed.
- Streaming persistence (persisting docs as each LLM call completes) - the full DAG must be complete before any works become visible to the coordinator, to preserve topological ordering.
- Reducing per-LLM-call latency.
- Changing the decomposer prompt templates.

## Proposed Solution

### Overview

Three independent changes, applied in order:

1. **Add `title` to `Doc`** - one-field struct change with a ripple fix in `double_write_old_records`.
2. **Async decomposition** - detach `decompose_hierarchy` into a Tokio task; add `Decomposing` FSM state to coordinator.
3. **Cross-scope dependency resolution** - thread a global `title_to_id` map through `decompose_hierarchy` so every `decompose_into` call can resolve dependencies against the full plan's accumulated docs.

### Architecture

#### Change 1: `title` field on `Doc`

**File:** `src/domain/doc.rs`

Add `pub title: String` to the `Doc` struct:

```rust
pub struct Doc {
    pub id: String,
    pub kind: DocKind,
    pub parent_id: Option<String>,
    pub title: String,          // NEW: human-readable title from LLM output
    pub markdown: String,
    pub dependencies: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
```

`Doc::new` gains a `title` parameter. Update the two call sites:

- `src/decomposer.rs:410` - `Doc::new(target_kind, Some(parent.id.clone()), filename)` becomes `Doc::new(target_kind, Some(parent.id.clone()), child.title.clone(), filename)`
- `src/daemon/handlers/doc.rs:184` - plan Doc uses `extract_plan_title(&markdown)` as its title (unchanged logic, now stored).

**File:** `src/daemon/handlers/doc.rs` (`double_write_old_records`)

Replace:
```rust
let child_title = extract_plan_title(&content);
```
With:
```rust
let child_title = child.title.clone();
```

This eliminates the re-extraction that fails for work docs. No other logic changes.

#### Change 2: Async decomposition

**New FSM state** - `src/domain/coordinator_state.rs`:

```rust
pub enum CoordinatorFsmState {
    Interviewing,
    Decomposing,        // NEW: waiting for background decomposition to complete
    Planning,
    ActivatePhase,
    Executing,
    PhaseGate,
    GoalComplete,
}
```

**`accept_plan_markdown`** - `src/daemon/handlers/doc.rs`:

Current structure:
```
create Plan Doc -> await decompose_hierarchy -> double_write -> create Goal -> start Coordinator -> return
```

New structure:
```
create Plan Doc -> create Goal (Decomposing) -> spawn task(decompose_hierarchy) -> return
                                                     |
                                                     v (background)
                                              decompose_hierarchy -> double_write -> emit decomposition.completed
```

The background task:
1. Runs `decompose_hierarchy` and `double_write_old_records`.
2. On completion (success or failure), emits a `DaemonEvent`:
   - Success: `decomposition.completed` with `{ "goal_id": "...", "child_count": N }`
   - Failure: `decomposition.failed` with `{ "goal_id": "...", "error": "..." }`
3. Does NOT write `CoordinatorState` directly. State ownership stays with the coordinator agent: the coordinator catches the event and drives its own transition from `Decomposing` to `Planning`.

The Tokio task needs `Arc<Stores>` and a clone of `broadcast::Sender<DaemonEvent>`. Both are already `Arc`/`Clone`.

**Coordinator FSM** - `src/agents/coordinator.rs` and `src/agents/coordinator/run.rs`:

Add `decomposition.completed` and `decomposition.failed` to `is_coordinator_wakeup` so the coordinator wakes early on those events instead of polling every 30 seconds.

Add transition logic in `compute_next_state`:
```rust
CoordinatorFsmState::Decomposing => {
    // Check if stores now contain phases (decomposition persisted them).
    // This is the signal that the background task completed.
    // The coordinator transitions itself; the background task does NOT write CoordinatorState.
    let plan = generation::find_active_plan(stores)?;
    let specs = generation::find_active_specs_for_plan(stores, &plan.id);
    let has_phases = specs
        .iter()
        .any(|s| !generation::find_active_phases_for_spec(stores, &s.id).is_empty());
    if has_phases { Some(CoordinatorFsmState::Planning) } else { None }
}
```

The `run_fsm_loop` idle-wait block adds `Decomposing` to the states that use event-driven wake:
```rust
CoordinatorFsmState::Planning
| CoordinatorFsmState::Decomposing    // NEW
| CoordinatorFsmState::ActivatePhase
| CoordinatorFsmState::PhaseGate => self.config.active_interval_secs,
```

The coordinator drives its own state transition. The background task only persists docs and emits an event. The coordinator, waking on `decomposition.completed`, checks the stores for persisted phases in `compute_next_state` and transitions itself to `Planning`.

**Return value from `doc.inject`/`doc.accept`:**

When `skip_decompose=true`, the response still returns `child_count: 0` synchronously (no change to test paths). When `skip_decompose=false`, `child_count` is omitted from the initial response (field absent or `null`). Callers must not rely on it for the async path.

#### Change 3: Cross-scope dependency resolution

**File:** `src/decomposer.rs`

The current per-call `title_to_id` map in `decompose_into` only covers siblings within one invocation. Elevate it to a shared map threaded through `decompose_hierarchy`.

```rust
pub async fn decompose_hierarchy<H: HttpClient + Sync>(
    plan: &Doc,
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &H,
    brief: bool,
) -> Result<Vec<Doc>> {
    let mut all_docs = Vec::new();
    let mut global_title_to_id: HashMap<String, String> = HashMap::new();  // NEW

    if brief {
        let works = decompose_into(plan, DocKind::Work, run_dir, config, http_client, &mut global_title_to_id).await?;
        all_docs.extend(works);
    } else {
        let specs = decompose_into(plan, DocKind::Spec, run_dir, config, http_client, &mut global_title_to_id).await?;
        for spec in &specs {
            let phases = decompose_into(spec, DocKind::Phase, run_dir, config, http_client, &mut global_title_to_id).await?;
            for phase in &phases {
                let works = decompose_into(phase, DocKind::Work, run_dir, config, http_client, &mut global_title_to_id).await?;
                all_docs.extend(works);
            }
            all_docs.extend(phases);
        }
        all_docs.extend(specs);
    }
    // ...
}
```

Inside `decompose_into`, after building the local sibling map, merge into `global_title_to_id` before resolving dependencies. Resolution checks local siblings first, then global scope:

```rust
doc.dependencies = dep_titles
    .iter()
    .filter_map(|title| {
        local_title_to_id.get(title)
            .or_else(|| global_title_to_id.get(title))
            .cloned()
            .or_else(|| {
                warn!("dependency '{}' not found in plan scope", title);
                None
            })
    })
    .collect();
```

After resolving, extend global map with this call's entries so future calls can reference them.

### Data Model

`Doc` struct changes (backward-compatible for new records; old records without `title` deserialize with `#[serde(default)]`):

```rust
#[serde(default)]
pub title: String,
```

`CoordinatorFsmState` gains `Decomposing`. It is a plain enum - no proc-macro derivation. Adding `Decomposing` requires only:
- A new arm in `compute_next_state` (returns `None` - coordinator holds and waits for event)
- Adding `Decomposing` to the idle-wait match arm in `run_fsm_loop`
- A `Decomposing => write!(f, "Decomposing")` arm in the `Display` impl

`CoordinatorState::new` is unchanged. In `accept_plan_markdown`, after calling `CoordinatorState::new(goal_id, InterviewMode::Skip)`, override `coord_state.fsm_state = CoordinatorFsmState::Decomposing` before persisting. This is a two-line change, explicit and localized. The manifest seed path creates state through `load_or_create_coordinator_state`, which does not go through `accept_plan_markdown`, and is unaffected.

The `run_iteration` handler for `Decomposing` must NOT call the LLM. It should immediately return `IterationOutcome::Done("waiting for decomposition to complete")`. The event-driven wake in `run_fsm_loop` handles the wake-up efficiently.

### API Design

No new IPC methods. `doc.inject` and `doc.accept` return immediately with:

```json
{
  "doc_id": "pl-xxxx",
  "run_dir": "/tmp/loopr-e2e/...",
  "child_count": null,
  "goal_id": "cg-xxxx",
  "coordinator_session_id": "ag-xxxx",
  "coordinator_already_running": false
}
```

New events on the event bus:
- `decomposition.completed` - `{ "goal_id": "cg-xxxx", "child_count": 32 }`
- `decomposition.failed` - `{ "goal_id": "cg-xxxx", "error": "..." }`

### Implementation Plan

**Phase 1: Title fix (self-contained)**
1. Add `title: String` to `Doc` with `#[serde(default)]`
2. Update `Doc::new` signature
3. Fix call site in `decomposer.rs`
4. Fix call site in `doc.rs` (plan doc)
5. Fix `double_write_old_records` to use `child.title`
6. Update all existing `Doc::new` calls in tests

**Phase 2: Async decomposition**
1. Add `Decomposing` to `CoordinatorFsmState`
2. Add `Decomposing` match arms to `compute_next_state`, `run_fsm_loop`, and `Display`
3. Refactor `accept_plan_markdown`: create goal in `Decomposing` state, spawn task, return
4. Background task: run decomposition, transition state, emit event
5. Coordinator: handle `Decomposing` idle-wait with event-driven wake

**Phase 3: Cross-scope dependency resolution**
1. Add `global_title_to_id: &mut HashMap<String, String>` to `decompose_into` signature
2. Merge local into global after each call
3. Update dependency resolution to check global scope
4. Update all callers of `decompose_into`

## Alternatives Considered

### Alternative 1: Streaming persistence
- **Description:** Persist each Doc immediately as the LLM generates it, allowing workers to start on the first phase's works while specs 2-3 are still being decomposed.
- **Pros:** Workers start ~6 minutes sooner; better utilization of the timeout budget.
- **Cons:** Works from phase N may declare dependencies on works from phase N+2 that don't exist yet. Coordinator would assign them without knowledge of those future deps, producing incorrect execution order.
- **Why not chosen:** Unsafe until dependency resolution is complete and verifiably sound. Can be revisited after Phase 3 is validated.

### Alternative 2: Parallel spec decomposition
- **Description:** Decompose all specs concurrently using `tokio::join_all`.
- **Pros:** Would reduce the 9-minute wall to ~3 minutes (parallelizing the 3 specs).
- **Cons:** Cross-spec dependency resolution becomes harder: a work in Spec A can't reference a work in Spec B if Spec B hasn't been generated yet. Requires a two-pass approach: generate all specs/phases/works first, then resolve all dependencies.
- **Why not chosen:** Higher complexity, dependency resolution design must be finalized first.

### Alternative 3: Increase E2E timeout
- **Description:** Bump `python-api` timeout from 1200s to 2400s.
- **Pros:** Zero code changes.
- **Cons:** Masks the structural problem; doesn't fix the IPC hang; doesn't fix the title or dependency bugs.
- **Why not chosen:** Not a fix.

## Technical Considerations

### Dependencies

- `CoordinatorFsmState`: plain enum, no proc-macro. Only match arm additions needed across `compute_next_state`, `run_fsm_loop`, and `Display`.
- `accept_plan_markdown`: creates `CoordinatorState` with `fsm_state` set to `Decomposing` directly, bypassing `CoordinatorState::new`'s `InterviewMode` logic. The manifest seed path is unchanged.
- Tests using `Doc::new` directly: all call sites updated to pass `title`.

### Performance

Background decomposition task runs on the Tokio thread pool. No new blocking is introduced. The task uses `ReqwestClient` which is already async. Wall-clock decomposition time is unchanged (still ~9 min for python-api); the improvement is that this time is no longer charged against the IPC caller.

### Security

No security implications. Background task operates within the same process and trust boundary as the handler it replaces.

### Testing Strategy

**Phase 1:**
- Unit: `test_doc_accept_skip_decompose_creates_doc` - add assertion that returned doc title != "Untitled Plan".
- Unit: `double_write_old_records` - new test asserting work titles match decomposer output.

**Phase 2:**
- Unit: coordinator FSM transitions - `Decomposing` -> `Planning` on event.
- Integration: `doc.accept` with `skip_decompose=true` returns immediately with `child_count: null`.
- Integration: after `decomposition.completed` event, coordinator advances to `Planning`.

**Phase 3:**
- Unit: `decompose_into` with cross-scope dependency - assert resolved, not dropped.
- Existing FSM tests updated to include `Decomposing` in valid state enum.

### Rollout Plan

Each phase is independently shippable. Phase 1 is a prerequisite for accurate E2E diagnosis. Phases 2 and 3 can be developed in parallel by different contributors.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Background task panics silently | Low | High | Wrap body in `async move { ... }.unwrap_or_else(...)` or use `tokio::spawn` with a join handle that emits `decomposition.failed` on any `Err` or panic |
| Daemon restart while decomposition is in flight | Med | High | On daemon startup, if any `CoordinatorState` has `fsm_state == Decomposing`, log a fatal warning and emit `decomposition.failed`; user must re-inject the plan |
| Two concurrent `doc.inject` calls | Low | Med | The existing goal-deactivation block at line 216 of `doc.rs` cancels the prior goal. With async, also cancel the in-flight background task for the prior goal. Store the `JoinHandle` in `DaemonContext` keyed by `goal_id`. |
| `Decomposing` not handled in exhaustive match arms throughout codebase | High | Low | `CoordinatorFsmState` is `Copy`; Rust compiler enforces exhaustive match. All existing `match coord_state.fsm_state` arms will fail to compile until `Decomposing` is added. This is the desired guard. |
| `Arc<Stores>` lock contention between background task and coordinator | Low | Low | Stores uses `RwLock`; background task only writes during persistence, coordinator only reads during `Decomposing` wait |
| `global_title_to_id` grows unbounded for very large plans | Low | Low | Map is per-decomposition-run, dropped when task completes; not a long-lived allocation |

## Open Questions

- [ ] Where should the in-flight `JoinHandle` for the background task be stored? `DaemonContext` is the natural home but it currently has no concept of per-goal tasks. An `Arc<Mutex<Option<JoinHandle<()>>>>` in `Stores` is simpler but pollutes the domain layer.
- [ ] Should `decomposition.failed` transition to `GoalComplete` (abandon, reuse existing terminal) or a new `DecompositionFailed` terminal state for observability in the TUI?
- [ ] Should `decomposition.failed` transition `CoordinatorState` to `GoalComplete` (abandon) or to a new `DecompositionFailed` terminal state for observability?
- [ ] Should `child_count` be surfaced via a `goal.status` IPC query rather than being dropped from the initial response? The TUI may depend on it.

## References

- `src/daemon/handlers/doc.rs` - `accept_plan_markdown`, `double_write_old_records`
- `src/decomposer.rs` - `decompose_hierarchy`, `decompose_into`
- `src/domain/coordinator_state.rs` - `CoordinatorFsmState`, `CoordinatorState`
- `src/agents/coordinator.rs` - `compute_next_state`
- `src/agents/coordinator/run.rs` - `run_fsm_loop`
- `src/domain/doc.rs` - `Doc` struct
- E2E run log: `/home/saidler/.local/share/loopr/sessions/20260405T232158/loopr.log`
