# Design Document: Coordinator Singleton + Parallel Decomposition

**Author:** Scott A. Idler
**Date:** 2026-04-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Three interconnected reliability fixes: enforce a true coordinator singleton so two
coordinators can never run simultaneously; parallelize sibling LLM calls in `decompose_hierarchy`
so the python-api target fits inside its 1200s budget; and make `double_write_old_records` hold
the TaskStore lock for its entire batch so a mid-write crash leaves a consistent (all-or-nothing)
database state.

## Problem Statement

### Background

The orchestration spine runs a single Coordinator agent that drives plan decomposition,
phase activation, and work assignment. The Decomposer is a background task that produces a
Plan → Spec → Phase → Work hierarchy by calling the LLM once per node. The resulting records
are double-written into both the new `Doc` store and the legacy Plan/Spec/Phase/Work stores via
`double_write_old_records`.

### Problem

Three bugs were confirmed in the 2026-04-05 python-api E2E run:

**Bug 1 — Dual coordinator:** Two coordinators (`ag-nv5k3` and `ag-am468`) ran simultaneously
throughout the entire run. `ag-nv5k3` was auto-started by the daemon at boot via
`auto_start_coordinator: true` in the e2e config. `ag-am468` was started by `doc_entry_pipeline`
510ms later. `auto_start_coordinator` is not a legitimate second path — it is a rogue code path
that competes with `doc.inject`/`doc.accept`, the only legitimate coordinator start point.
The pool check also failed to block the second start because `store.create(session)` happens
outside the `write_agent_sessions()` lock, creating a window where the in-memory map is
inconsistent with durable state.

**Bug 2 — Sequential decomposition timeout:** `decompose_hierarchy` processes nodes one at a
time: plan → spec₁ → (phases → works) → spec₂ → ... → spec₃. For python-api this produced
22+ sequential LLM calls. Each call takes 5-30s; parse-failure retries add 3+ minutes each.
The target timed out at 1200s with the last phase still decomposing. The coordinator never
received `decomposition.completed`.

**Bug 3 — Non-atomic double_write:** `double_write_old_records` calls `store.create()` once
per record (22+ calls total). Each call opens the JSONL file, acquires an exclusive lock,
writes, syncs, and releases — independently. A process crash or error after record 15 of 22
leaves Plans, Specs, and some Phases in the DB but no Works. The coordinator transitions to
Decomposing and receives `decomposition.failed`, but the DB is now in a state where partial
Specs and Phases exist with no corresponding Works and no parent relationship integrity.

### Goals

- **G1:** Eliminate the dual coordinator by removing `auto_start_coordinator` entirely —
  `doc.inject`/`doc.accept` is the sole legitimate coordinator start path. Fix the pool
  check's write-ordering so the in-memory map and TaskStore are updated atomically.
- **G2:** `decompose_hierarchy` completes the python-api hierarchy (3 specs, 6 phases, 12
  works) within 600s under normal LLM latency.
- **G3:** `double_write_old_records` holds the TaskStore lock for its entire batch; no
  external reader observes a half-written hierarchy during the write window.
- **G4:** Each LLM call inside the decomposer is bounded; a stalled HTTP connection cannot
  hang the decomposer indefinitely.

### Non-Goals

- Changing the TaskStore external crate to add a native batch API.
- Parallelizing ratification calls in `ratify_hierarchy` (those are sequential by design —
  each parent ratifies after seeing all its children).
- Parallelizing works within a phase (works have intra-phase dependency edges; safe
  parallelism requires a topological sort pass which is out of scope here).
- Adding rate-limit backoff for Anthropic 429 responses (follow-on concern).

## Proposed Solution

### Overview

**Fix 1 — Remove `auto_start_coordinator`; fix write ordering in `handle_agent_start`:**
Delete `auto_start_coordinator` from config, daemon startup, and all tests. There is one
legitimate path to start a coordinator: `doc.inject`/`doc.accept` → `doc_entry_pipeline` →
`agent.start`. Crash recovery is already handled by the supervisor reacting to `Failed` events
emitted by `recover_orphaned_records` on daemon restart — `auto_start_coordinator` is
redundant and dangerous.

Fix the pool check write ordering per the TaskStore data architecture rule (see
`.claude/rules/taskstore.md`): move `store.create(session)` inside the
`write_agent_sessions()` lock, after the pool check, before `sessions.insert()`. TaskStore is
source of truth; in-memory map is a read replica. The correct sequence is:

1. Acquire `write_agent_sessions()` lock
2. Check pool count — reject if at capacity
3. While holding the lock: acquire TaskStore lock, call `store.create(session)`
4. If `store.create()` succeeds: `sessions.insert(id, session)`
5. Drop the lock

**Fix 2 — Parallel sibling decomposition with LLM timeout:** Refactor `decompose_hierarchy`
to run sibling groups concurrently using `futures::future::try_join_all`. Specs are siblings
(all children of the plan), so they run concurrently on the same async task — no `tokio::spawn`
required, so no `Send` bound issues with borrowed data. Phases within each spec are siblings and
run concurrently in the same way. Each concurrent branch builds a local `title_to_id` map;
after all branches complete, the local maps are merged into the shared `global_title_to_id` for
cross-spec dependency resolution — no `Arc<Mutex>` needed. Wrap every LLM HTTP call in
`tokio::time::timeout(Duration::from_secs(LLM_CALL_TIMEOUT_SECS))`. A timeout is treated as a
parse failure and triggers the existing one-shot retry; if the retry also times out,
`decompose_into` returns `Err` and `try_join_all` propagates the first error.

**Fix 3 — Single-lock batch write:** Refactor `double_write_old_records` into two phases.
Phase A: build all old records in memory (Plan, Specs, Phases, Works) and resolve all
parent-ID cross-references. This is pure in-memory work; if any step fails (missing parent,
serialization error), the function returns `Err` before touching the DB. Phase B: acquire
`store_arc.lock()` once and call `store.create()` for every record while holding that single
lock. The practical guarantee is: no other daemon writer can observe a half-written hierarchy
during the write window (coordinator FSM updates, work status transitions, etc. all block
until the batch lock is released). This is not crash-atomicity — a process crash mid-batch
still leaves partial JSONL, and records written before the failure are committed. This is
inherent to the JSONL format; `recover_orphaned_records` handles the surviving partial state
on the next daemon startup by marking orphaned sessions as Failed and allowing the coordinator
to retry decomposition.

### Architecture

#### Fix 1 — Remove `auto_start_coordinator`; fix write ordering

Remove from `src/daemon.rs`:
```rust
// DELETE the entire Gap #31 block:
// if c.config.agents.auto_start_coordinator { ... }
```

Remove from `src/config.rs`:
```rust
// DELETE: auto_start_coordinator field from AgentConfig
// DELETE: all default/test values referencing it
```

`handle_agent_start` — new write ordering:
```
1. acquire write_agent_sessions lock
2. check pool count — return pool_exhausted if at capacity
3. acquire store_arc.lock() [TaskStore mutex, while still holding sessions lock]
4. store.create(session)?  — if Err, return RpcError::internal
5. sessions.insert(id, session)
6. drop store lock, drop sessions lock
7. spawn task
```

This ordering guarantees: if the process crashes after step 4 and before step 5, JSONL has
the record and it will be replayed on restart. The previous ordering (insert into memory, then
write to TaskStore) left a window where a crash produced a ghost record in memory with no
durable backing.

#### Fix 2 — Parallel decomposition

Current call graph (sequential):
```
decompose_hierarchy(plan)
  decompose_into(plan → spec₁)  // ~20s
  decompose_into(plan → spec₂)  // ~20s
  decompose_into(plan → spec₃)  // ~20s
  for spec₁: decompose_into(spec₁ → phase₁)  // ~15s
             decompose_into(spec₁ → phase₂)  // ~15s
  ...
  total: ~22 sequential calls × ~15s = ~330s minimum, unbounded with retries
```

New call graph (concurrent siblings):
```
decompose_hierarchy(plan)
  try_join_all([
    decompose_spec_branch(spec₁),   // parallel
    decompose_spec_branch(spec₂),   // parallel
    decompose_spec_branch(spec₃),   // parallel
  ])
  post_pass: resolve_cross_spec_deps(&mut all_docs, &global_title_to_id)
  ratify_hierarchy(...)  // unchanged, sequential bottom-up

decompose_spec_branch(spec, run_dir, config, http_client)
  phases = try_join_all([
    decompose_phase_branch(phase₁),  // parallel (same task, not spawned)
    decompose_phase_branch(phase₂),  // parallel
    ...
  ])
  // each branch returns (docs, local_title_to_id)
  merge local maps into branch_title_to_id
  return (spec, all_works, branch_title_to_id)
```

After `try_join_all` on specs completes, merge all `branch_title_to_id` maps into
`global_title_to_id`, then do a single cross-spec dependency resolution pass over all docs.
No concurrent mutation — merge happens sequentially after all branches finish.

Every LLM call site in `decompose_into` (the `call_llm` / streaming call):
```rust
tokio::time::timeout(
    Duration::from_secs(LLM_CALL_TIMEOUT_SECS),
    call_llm_for_decomposition(...)
)
.await
.map_err(|_| eyre!("LLM call timed out after {}s", LLM_CALL_TIMEOUT_SECS))?
```

`const LLM_CALL_TIMEOUT_SECS: u64 = 60;`

#### Fix 3 — Single-lock batch write

```rust
fn double_write_old_records(...) -> eyre::Result<()> {
    // Phase A: serialize all records (fails fast before touching DB)
    let records = build_all_old_records(plan_doc, plan_markdown, child_docs, run_dir)?;

    // Phase B: hold the store lock for the entire batch
    if let Some(store_arc) = &stores.store {
        let mut store = store_arc.lock()
            .map_err(|_| eyre!("taskstore lock poisoned"))?;
        for record in &records {
            record.create_in(&mut store)?;   // fails → Err; prior records committed but
                                             // no concurrent reader saw partial state
        }
        // lock drops here
    }

    // Phase C: update in-memory maps (same as before)
    for record in records {
        record.insert_in_memory(stores)?;
    }
    Ok(())
}
```

`build_all_old_records` serializes every Plan/Spec/Phase/Work to a typed enum `OldRecord`
(no DB I/O). If serialization or parent-id resolution fails, it returns `Err` before any DB
write has happened.

### Data Model

No new persistent fields. The `coordinator_token: Arc<AtomicBool>` lives in memory only —
it is always `false` on daemon startup (correct: old sessions are marked Failed by
`recover_orphaned_records` before any new starts are attempted).

### API Design

`Stores::new()` initializes `coordinator_token: Arc::new(AtomicBool::new(false))`.

`Stores::claim_coordinator_token() -> bool` — tries `compare_exchange(false, true)`, returns
true if claimed, false if already held. Used only by `handle_agent_start`.

`Stores::release_coordinator_token()` — stores `false`. Called by executor on coordinator
task exit.

No IPC protocol changes. No config schema changes.

### Implementation Plan

**Phase 1 — Remove `auto_start_coordinator`; fix write ordering**
- Delete `auto_start_coordinator` field from `AgentConfig`, its default, all config tests,
  all e2e target `loopr.yml` templates, and the Gap #31 block in `src/daemon.rs`
- In `handle_agent_start`: move `store.create(session)` inside the `write_agent_sessions()`
  lock, after the pool check, before `sessions.insert()` — per `.claude/rules/taskstore.md`
- Apply the same write-ordering fix to any other `handle_*` functions that currently insert
  into in-memory maps before calling `store.create()`
- Tests: `test_agent_start_pool_check_atomic_under_lock`,
  `test_no_auto_start_coordinator_on_daemon_boot`

**Phase 2 — Per-call LLM timeout (lowest risk, highest leverage)**
- Add `const LLM_CALL_TIMEOUT_SECS: u64 = 60` to `decomposer.rs`
- Wrap the LLM streaming call in `tokio::time::timeout(...)`
- Treat timeout as parse failure (triggers existing one-shot retry)
- Tests: `test_decompose_into_times_out_slow_llm`

**Phase 3 — Parallel sibling decomposition**
- Add `futures` crate (`cargo add futures`)
- Refactor `decompose_hierarchy`: extract `decompose_spec_branch(spec, run_dir, config,
  http_client) -> Result<(Vec<Doc>, HashMap<String,String>)>` helper
- Replace sequential loops with `try_join_all`; each branch returns its local `title_to_id`
  map; merge maps after `try_join_all` returns; run cross-spec dep resolution in one pass
- No `Arc<Mutex>` needed — branches are polled on the same async task, not spawned
- Tests: `test_decompose_hierarchy_parallel_specs_complete`,
  `test_decompose_hierarchy_cross_spec_deps_resolved`

**Phase 4 — Atomic double_write**
- Add `enum OldRecord { Plan(Plan), Spec(Spec), Phase(Phase), Work(Work) }` with a
  `create_in(&mut Store) -> Result<String>` method
- Extract `build_all_old_records(plan_doc, plan_markdown, child_docs, run_dir)
  -> Result<Vec<OldRecord>>` — pure in-memory; resolves parent IDs; fails fast before any
  DB I/O
- Refactor `double_write_old_records` to hold the store lock for the entire batch;
  after the batch succeeds, update in-memory maps as before
- Tests: `test_double_write_no_interleaved_reads_on_failure`,
  `test_build_all_old_records_fails_on_missing_parent`

## Alternatives Considered

### Alternative 1: Keep `auto_start_coordinator`, add singleton guard

- **Description:** Keep the feature but add an `Arc<AtomicBool>` coordinator token to
  `Stores`; second start attempt fails the `compare_exchange`.
- **Pros:** Preserves `auto_start_coordinator` for clean-restart resume.
- **Cons:** Treats two competing paths as both legitimate and adds complexity to paper over
  the conflict. Clean-restart resume is already handled by the supervisor reacting to
  `Failed` events from `recover_orphaned_records`. The feature is redundant.
- **Why not chosen:** One path is the fix. Defending against two paths with a lock is
  the wrong abstraction.

### Alternative 2: `try_join_all` for works within a phase

- **Description:** Also parallelize work generation within each phase.
- **Pros:** Maximum parallelism.
- **Cons:** Works within a phase can have dependency edges on each other (work A must
  complete before work B). Running them concurrently produces correct docs but the
  dependency resolution in `global_title_to_id` may produce incorrect work-ID cross-refs if
  sibling works reference each other. Safe to add in a follow-on once the phase parallelism
  is validated.
- **Why not chosen:** Out of scope for this doc. Phases → works are the most numerous
  sequential calls; spec and phase parallelism already covers the bulk of the latency.

### Alternative 3: Taskstore batch API (upstream contribution)

- **Description:** Add a `create_batch<T: Record>(records: Vec<T>)` method to the TaskStore
  crate that writes all records in a single SQLite transaction and a single JSONL append.
- **Pros:** True atomicity.
- **Cons:** Requires changing an external crate; scope creep; JSONL is still append-only so
  true crash-atomicity requires a write-ahead log.
- **Why not chosen:** The single-lock approach achieves the practical goal (no interleaving)
  without touching the crate.

## Technical Considerations

### Dependencies

- `futures` crate — needed for `try_join_all`. Add via `cargo add futures`.
- No other new dependencies.

### Performance

- Phase 2 (timeout): no overhead on the happy path; `tokio::time::timeout` has negligible
  cost.
- Phase 3 (parallel): python-api 3 specs × 4 phases = 12 concurrent LLM calls at peak.
  Each is a streaming HTTP connection. The Reqwest client is already shared; 12 concurrent
  connections are within normal HTTP client limits. Anthropic rate limits may apply — if
  429s occur, `decompose_into` returns Err and `decompose_hierarchy` propagates failure.
  Rate-limit backoff is a follow-on concern.
- Phase 4 (batch lock): the store lock is held for longer (~22 × JSONL fsync duration).
  Other writers (coordinator FSM updates) will block during this window. The window is
  bounded by the number of records (~22) and JSONL fsync latency (~1ms each) = ~22ms worst
  case. Acceptable.

### Security

No security implications. All changes are internal to the daemon process.

### Testing Strategy

Each phase has its own unit tests listed in the Implementation Plan. Integration coverage:

- `test_coordinator_token_blocks_second_start`: call `agent.start` twice for coordinator;
  second must return pool_exhausted.
- `test_coordinator_token_released_on_terminal`: start coordinator, force terminal status,
  verify token is released, verify a third `agent.start` succeeds.
- `test_decompose_hierarchy_parallel_completes_within_timeout`: mock LLM that sleeps 100ms
  per call; sequential would take 2200ms, parallel must complete in ~400ms.
- `test_double_write_no_interleaved_reads_on_failure`: inject a failing store on record 10;
  verify the function returns Err and the coordinator receives `decomposition.failed`.

### Rollout Plan

Each phase is independently shippable. Suggested order: Phase 1 (highest urgency, smallest
change) → Phase 2 (no architecture change) → Phase 3 (largest refactor) → Phase 4.
`otto ci` must pass after each phase before proceeding.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Token not released on coordinator panic | Low | High | `run_agent_task` catches panics and forces terminal status; token release is in the terminal path |
| Parallel LLM calls hit Anthropic rate limit (429) | Medium | Medium | `decompose_into` returns Err on 429; `try_join_all` propagates first error; `decomposition.failed` wakes coordinator for retry |
| Cross-spec dependency resolution incorrect after parallelism | Low | Medium | Post-processing pass over all docs; unit test with known cross-spec dep graph |
| Store lock held 22ms blocks coordinator FSM update | Low | Low | 22ms is imperceptible; coordinator polls every 30s |
| `futures` crate version conflict | Low | Low | `cargo add futures` pulls latest; check `cargo tree` after add |

## Open Questions

- [ ] Should `auto_start_coordinator: true` be silently downgraded to a no-op when the token
  is already held (silent skip) vs. logging a warning? The token approach makes it safe either
  way, but the behavior should be explicit.
- [ ] When `try_join_all` returns the first error from a failing spec branch, the other
  branches are dropped (cancelled). Should partial results be salvageable, or is all-or-nothing
  on decomposition the right policy?

## References

- `src/daemon/handlers/agent.rs` — `handle_agent_start`, pool check
- `src/daemon/handlers/doc.rs` — `doc_entry_pipeline`, `double_write_old_records`
- `src/decomposer.rs` — `decompose_hierarchy`, `decompose_into`
- `src/daemon/supervisor.rs` — coordinator restart logic
- `src/daemon.rs:278` — `auto_start_coordinator` startup path
- `src/agents/executor/` — agent task lifecycle, terminal transitions
- E2E run 2026-04-05: `ag-nv5k3` + `ag-am468` dual coordinator; python-api 9-minute hang
