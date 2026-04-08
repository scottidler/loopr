# Design Document: E2E Parallelism and Recovery Fixes

**Author:** Scott A. Idler
**Date:** 2026-04-08
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The python-api E2E run (2026-04-08) completed 9/15 work items in 1200s before timing out. Three root causes limited throughput: the decomposer produced serial dependency chains instead of parallel work items, the reconciler mishandled a "bundle merged but work stuck InProgress" edge case, and the implementer's absolute-path error cascaded into a coordinator crash. This document covers six fixes across decomposition quality, worker scaling, reconciler logic, coordinator recovery, and implementer error feedback.

## Problem Statement

### Background

Loopr's execution model is: Plan -> Specs (sequential by order) -> Phases (sequential within each Spec) -> Work items (parallel within a Phase, constrained by deps and file locks). Workers are pull-based: they poll `work_queue::next_assignable_work()` for Ready items in the current active Phase.

The python-api target decomposes into 3 Specs, each with 1 Phase. Workers filter by `current_phase_id`, so only one Phase's work items are available at a time. Within a Phase, parallelism depends on the dependency graph the decomposer produces.

### Problem

**Observed:** 15 work items executed nearly serially despite 2 workers and many items with no file contention.

**Root causes:**

1. The Database Layer Phase had 5 work items in a strict linear chain (A->B->C->D->E). With 2 workers and a 5-deep chain, only 1 item can be InProgress at a time. These should have been 1-2 work items.
2. All 15 work items had `files: []` - the decomposer does not emit file lists, so the advisory lock system had nothing to work with.
3. Worker pool is hardcoded at 2 regardless of how many items could run in parallel.
4. When `wk-r9fcf`'s implementer hit an absolute-path sandbox error after the bundle was already committed and merged, the work item got stuck InProgress with no active session. The reconciler moved it to Blocked instead of recognizing the Merged bundle and advancing to Done.
5. The coordinator tried `InProgress -> Done` (invalid for coordinator role) 3 times, then crashed via the lifeguard.
6. The implementer's error on absolute path rejection did not self-correct effectively, leading to the cascading failure.

### Goals

- Work items within a Phase are independently implementable by default; deps are the exception
- The `files` field is populated by the decomposer so advisory locks function
- Worker pool scales to match available parallelism
- Reconciler correctly advances work to Done when a Merged bundle exists
- Coordinator has a valid recovery path for stuck InProgress work with Merged bundles
- Implementer error feedback for absolute path rejection is clearer

### Non-Goals

- Changing Spec or Phase ordering from sequential to parallel (future consideration)
- Dynamic worker pool resizing at runtime (just make it configurable and set a sensible default)
- Changing the sandbox path validation rules

## Proposed Solution

### Fix 1: Decomposition - parallelizable work items

**File:** `prompts/decompose/work.pmt`

Add a "Parallelism" section to the Rules:

```
## Parallelism

Work items are discrete chunks that can be built independently and in parallel.
This is their primary design purpose. When decomposing:

- Most work items should have NO dependencies. If an implementer needs the
  output of another work item to start, that is a dependency. If they just
  need to know the interface contract (function signatures, API shape), that
  is NOT a dependency - the contract is already defined in the Spec.
- A dependency chain of 3+ items (A -> B -> C) is a signal you have
  over-decomposed. Collapse the chain into one work item.
- Each work item must list the files it will create or modify in the
  "files" field. This enables the orchestrator to detect conflicts and
  schedule non-overlapping items in parallel.
```

Update Rule 5 from "Produce 1-5 Work items for typical Phases" to:

```
5. Produce 1-5 Work items per Phase. Prefer fewer, larger items over many
   small serial items. Two independent items beat five dependent ones.
```

**Expected outcome for python-api:** The Database Layer Phase would produce 1-2 work items (e.g., "Implement database.py" as a single item) instead of 5 chained items. The API Routes Phase might produce 2 items ("Pydantic models + app scaffold" and "All endpoint handlers") with no deps between them if they touch different files.

### Fix 2: Decomposition - files field in output schema

**Files:** `prompts/decompose/work.pmt`, `src/decomposer.rs`

Update the output JSON schema in `work.pmt` to include `files`:

```json
[
  {
    "title": "Short descriptive title",
    "content": "Full markdown content following the Work template",
    "dependencies": [],
    "acceptance_criteria": ["criterion 1", "criterion 2"],
    "files": ["database.py", "test_database.py"]
  }
]
```

Add `files` rule:

```
7. Each Work item must include a "files" array listing every file it will
   create or modify. The orchestrator uses this to detect conflicts and
   schedule non-overlapping items concurrently.
```

In `src/decomposer.rs`:

1. Add `files` to `ChildEntry`:
   ```rust
   #[serde(default)]
   pub files: Vec<String>,
   ```

2. Add `files` to `ChildRecord` struct (alongside `dependencies`, `acceptance_criteria`).

3. Wire `files` through to `Work` construction in `persist_hierarchy()`:
   ```rust
   DocKind::Work => {
       let mut work = Work::new(parent_id, child.title.clone());
       // ... existing fields ...
       work.files = child.files.clone();
       works.push(work);
   }
   ```

4. Add `files` to the JSON schema in the decomposer prompt builder (the `serde_json::json!` block around line 210-230).

### Fix 3: Worker pool scaling

**Files:** `src/config.rs`, `src/daemon.rs`

Replace the `u32` `worker_pool_size` field with a `WorkerPoolSize` enum that accepts a numeric value, `"auto"`, or `"nproc"` from config. `auto` and `nproc` both resolve to `std::thread::available_parallelism()` at daemon startup. No extra crate needed.

```rust
/// Number of pull-based worker tasks. Accepts a fixed count or "auto"/"nproc"
/// to use the host's available parallelism.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WorkerPoolSize {
    Named(String),  // "auto" or "nproc"
    Fixed(u32),
}

impl WorkerPoolSize {
    pub fn resolve(&self) -> u32 {
        match self {
            WorkerPoolSize::Fixed(n) => *n,
            WorkerPoolSize::Named(_) => {
                std::thread::available_parallelism()
                    .map(|n| n.get() as u32)
                    .unwrap_or(4)
            }
        }
    }
}

impl Default for WorkerPoolSize {
    fn default() -> Self {
        WorkerPoolSize::Named("auto".to_string())
    }
}
```

YAML config stays natural:

```yaml
agents:
  worker-pool-size: auto   # or: nproc, or: 4
```

In `src/daemon.rs`, replace `c.config.agents.worker_pool_size` with `c.config.agents.worker_pool_size.resolve()` at the spawn site.

The worker loop, work queue, and atomic claim logic already handle arbitrary pool sizes - no other changes needed.

### Fix 4: Reconciler - Merged bundle means Done

**File:** `src/daemon/context.rs`

The reconciler's Work sweep (line ~728) currently does:
```
Work: InProgress with no active session -> Blocked
```

Change to: before setting Blocked, check if the work has a Merged bundle. If so, advance to Done instead.

```rust
// --- Work: InProgress with no active session ---
{
    // Read bundles BEFORE acquiring works write lock (lock ordering safety)
    let merged_work_ids: HashSet<String> = self.stores.read_bundles()
        .map(|bundles| {
            bundles.values()
                .filter(|b| b.status() == BundleStatus::Merged)
                .map(|b| b.work_id.clone())
                .collect()
        })
        .unwrap_or_default();

    let Ok(mut works) = self.stores.write_works() else {
        warn!("Reconciliation: cannot write works, skipping");
        return fixed;
    };
    // ... existing code ...
    for (id, wi) in works.iter_mut() {
        if wi.status() != WorkStatus::InProgress { continue; }
        if active_work_ids.contains(id) { continue; }

        if merged_work_ids.contains(id) {
            warn!("Reconciliation: Work {} InProgress with Merged bundle, advancing to Done", id);
            wi.force_status(WorkStatus::Done);
        } else {
            warn!("Reconciliation: Work {} InProgress with no active session", id);
            wi.force_status(WorkStatus::Blocked);
        }
        // ... persist ...
    }
}
```

Lock ordering: `read_bundles()` is acquired and released before `write_works()`. The merged work IDs are collected into a `HashSet` so the bundles lock is not held during the work mutation. If `read_bundles()` fails, we fall back to an empty set (conservative: no Merged bundles detected, existing Blocked behavior preserved).

### Fix 5: Coordinator recovery for stuck InProgress + Merged bundle

**File:** `src/agents/coordinator.rs`

The coordinator's current failure mode: it sees Work InProgress with a Merged bundle, tries `transition InProgress -> Done`, gets rejected (invalid for coordinator role), retries 3 times, then crashes.

The fix is to use `OverrideWork` instead of `transition` when the coordinator detects this case. `OverrideWork` bypasses normal FSM rules. The coordinator already has this action available.

However, the better fix is for the reconciler (Fix 4) to handle this case before the coordinator ever sees it. The reconciler runs every 60s. If the reconciler correctly advances Merged-bundle work to Done, the coordinator never encounters the stuck state.

Fix 4 is the primary fix. Fix 5 is required alongside it because the reconciler runs on a 60s interval - the coordinator may encounter the stuck state before the next sweep. Update the coordinator prompt (`prompts/coordinator.pmt`) to instruct:

```
If a Work is InProgress but its bundle is Merged, do NOT attempt a transition
action. The reconciler will advance it to Done automatically. Move on to other
work items.
```

This prevents the coordinator from crashing while the reconciler catches up.

### Fix 6: Implementer absolute path error feedback

**File:** `prompts/implementer.pmt`

The implementer used `/tmp/loopr/e2e/python-api/20260408-102524/main.py` instead of `main.py`. The sandbox correctly rejected it, but the LLM tried a different absolute path on the next iteration instead of switching to relative.

Add to the implementer prompt:

```
## Path rules

All file paths in read, write, edit, and commit actions must be RELATIVE to
the worktree root. Never use absolute paths. If a tool returns an "absolute
paths not allowed" error, retry with just the filename or relative path
(e.g., "main.py" not "/tmp/.../main.py").
```

The sandbox error message (`absolute paths not allowed: /tmp/...`) already names the offending path. The prompt addition ensures the LLM knows the fix is to use relative paths, not to try a different absolute path.

## Alternatives Considered

### Alternative 1: Make Specs parallel instead of sequential
- **Description:** Remove `spec.order` and let Specs within a Plan execute concurrently.
- **Pros:** Maximum parallelism - Database Layer and Test Suite could run simultaneously.
- **Cons:** Higher merge conflict risk (multiple Specs writing to overlapping files). Requires the integrator to handle more complex merge scenarios. The advisory lock system would need to be the primary conflict resolution mechanism.
- **Why not chosen:** The decomposition quality fix (Fix 1) addresses the immediate throughput problem. If Phases produce fewer, more independent work items, serial Specs with parallel work items is sufficient. Spec-level parallelism remains a future option if needed.

### Alternative 2: Dynamic worker pool resizing
- **Description:** Monitor the number of Ready items and spawn/kill workers dynamically.
- **Pros:** Optimal resource usage. Never idle workers, never bottleneck on pool size.
- **Cons:** Complexity: tokio task lifecycle management, graceful shutdown of excess workers, race conditions during scaling. Worker pool size is rarely the bottleneck - dependency chains are.
- **Why not chosen:** A static pool of 4 covers the common case. The real bottleneck is dependency graph shape, which Fix 1 addresses. Dynamic scaling is over-engineering for the current problem.

### Alternative 3: Coordinator directly advances InProgress -> Done
- **Description:** Add `InProgress -> Done` as a valid FSM transition for coordinator role.
- **Pros:** Simple. Coordinator can fix stuck work items directly.
- **Cons:** Violates the principle that work must pass through InReview and integration before Done. A coordinator doing InProgress -> Done bypasses review entirely. This opens a hole where untested code reaches Done status.
- **Why not chosen:** The reconciler fix (Fix 4) is the correct place - it checks for evidence of completion (Merged bundle) before advancing. The FSM rules should remain strict.

## Technical Considerations

### Dependencies

- Fix 1 and Fix 2 are prompt/schema changes - no Rust dependencies.
- Fix 2 adds a `files` field to `ChildEntry` and `ChildRecord` - backward-compatible (serde default).
- Fix 3 changes a config default - backward-compatible (overridable in loopr.yml).
- Fix 4 requires reading bundles during the reconciler's work sweep - adds a read lock acquisition.
- Fix 5 is a prompt change only.
- Fix 6 is a prompt change only.

### Performance

- Fix 3 (4 workers instead of 2): doubles idle polling when no work is available. Cost is negligible (4 idle tokio tasks sleeping).
- Fix 4 (read bundles in reconciler): one additional read lock per reconciliation sweep (every 60s). Negligible.

### Testing Strategy

**Fix 1 and Fix 2:** Re-run the python-api E2E target. Success criteria:
- Database Layer Phase produces <= 2 work items
- Work items have non-empty `files` arrays
- Multiple workers execute items concurrently within a Phase

**Fix 3:** Verify 4 workers spawn in logs: `Spawning 4 pull-based workers`

**Fix 4:** Add a unit test to `context.rs`:
- Create a Work at InProgress with no active session
- Create a Bundle at Merged for that Work
- Run `reconcile()`
- Assert Work is Done (not Blocked)

Also add the inverse test:
- Create a Work at InProgress with no active session
- No Merged bundle
- Run `reconcile()`
- Assert Work is Blocked

**Fix 5:** Covered by Fix 4 testing. The coordinator prompt change is defense-in-depth.

**Fix 6:** Covered by E2E re-run. Watch for the implementer self-correcting after an absolute path error.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Decomposer produces too few items (one giant Work) | Medium | Medium | The .pmt says 1-5 items and "prefer fewer, larger." If items get too large, the implementer may struggle. The "1-5" range provides a floor. |
| files field is inaccurate (LLM guesses wrong) | Medium | Low | Advisory locks are soft - they affect priority, not availability. Wrong files = suboptimal scheduling, not broken execution. |
| 4 workers overwhelm LLM API rate limits | Low | Medium | Workers sleep between polls. If rate-limited, the LLM client already retries. |
| Reconciler advances Work to Done prematurely (Merged bundle but code was bad) | Low | Low | The bundle was Merged by the integrator, which means the code passed review AND integration. If the code is bad, that's a reviewer/integrator failure, not a reconciler failure. |

## Open Questions

- [ ] Should the `files` field in the JSON schema sent to the LLM include the field description in the tool-use schema block (line ~210 of decomposer.rs)?
- [ ] Should worker_pool_size be 4 globally or configurable per-target in the e2e scripts?

## References

- E2E run log: `/tmp/loopr/e2e/python-api/20260408-102524/`
- Session log: `~/.local/share/loopr/sessions/20260408T172614/loopr.log`
- Work queue: `src/daemon/work_queue.rs`
- Worker loop: `src/agents/worker.rs`
- Reconciler: `src/daemon/context.rs` (line ~588, `reconcile()`)
- Sandbox: `src/agents/sandbox.rs`
- Decomposer: `src/decomposer.rs`
- Work FSM: `src/domain/work.rs`
- Coordinator actions: `src/agents/executor/action/work.rs`
