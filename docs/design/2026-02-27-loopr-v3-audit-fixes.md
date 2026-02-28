# Design Document: Loopr v3 Audit Defect Fixes

**Author:** Scott Aidler + Claude
**Date:** 2026-02-27
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

A comprehensive audit of Loopr v3 (MVP3+MVP4 implementation) against the three design conversations revealed 23 defects across four severity tiers. This document specifies the fix for every defect — 3 HIGH (behavioral correctness), 10 MEDIUM (missing features, fields, invariants), and 5 LOW — organized into four implementation phases that can be executed incrementally without destabilizing the existing system.

## Problem Statement

### Background

Loopr v3 MVP4 implementation is largely complete: 10 domain record types, 5 agent types, FSM transition tables, single-writer dispatch, NDJSON IPC, TUI thin client, CLI with auto-daemon, worktree management, and the Integrator pipeline. The core architecture is solid.

However, a line-by-line audit against the design conversations surfaced 23 gaps ranging from resource leaks (worktree cleanup never called) to missing handler guards (Tick singleton, version validation) to unenforced invariants (Work assignee, cycle detection, Bundle uniqueness).

### Problem

Three categories of defects threaten production readiness:

1. **Resource leaks and data loss** — Worktrees accumulate indefinitely (#1), Researcher loses multi-iteration context (#2), Integrator Tick fields vanish on restart (#3).
2. **Missing promised behavior** — FSM transitions, handler guards, version validation, and graceful shutdown that the design documents specify but the code omits (#4-#8).
3. **Unenforced invariants** — Missing struct fields (#9-#12) and missing validation logic (#13-#18) that allow invalid state to be persisted.

### Goals

- Fix all 3 HIGH defects (correctness blockers)
- Fix all 5 MEDIUM missing-feature defects (#4-#8)
- Add all 4 missing struct fields (#9-#12)
- Enforce all 6 missing invariants (#13-#18)
- Fix all 5 LOW defects (#19-#23)
- Every fix includes tests
- No regressions — `otto ci` passes after each phase

### Non-Goals

- New features beyond what the design docs specify
- Refactoring code unrelated to the 23 defects
- Performance optimization
- Proposal/Decision record types (#22) get scaffolding only — full implementation is future work

## Proposed Solution

### Overview

Four phases, ordered by severity and dependency:

| Phase | Scope | Defects |
|-------|-------|---------|
| **P1: Critical Fixes** | Resource leaks, data loss, context loss | #1, #2, #3 |
| **P2: FSM & Handler Hardening** | Missing transitions, singleton guard, version check, shutdown | #4, #5, #6, #7, #8 |
| **P3: Schema & Invariants** | Missing fields + enforcement logic | #9-#18 |
| **P4: Low-Priority Cleanup** | Bundle description, reviewer worktree, request_changes, Proposal/Decision stubs, TUI Locks tab | #19-#23 |

### File Reference

Every defect maps to specific source files. This table shows where to make changes:

| Defect | Primary File(s) | Secondary File(s) |
|--------|----------------|-------------------|
| #1 | `src/agents/executor.rs` | `src/agents/bridge.rs`, `src/worktree/manager.rs` |
| #2 | `src/agents/researcher.rs` | — |
| #3 | `src/agents/integrator_task.rs` | `src/daemon/handlers.rs` (tick transition persist) |
| #4 | `src/domain/work.rs` | — |
| #5 | (no code change) | `docs/design/` (document the decision) |
| #6 | `src/daemon/handlers.rs` | `src/domain/tick.rs` (add `is_terminal()`) |
| #7 | `src/daemon/handlers.rs` | — |
| #8 | `src/daemon/mod.rs` | `src/daemon/context.rs` (store JoinHandles) |
| #9 | `src/domain/work.rs` | `src/daemon/handlers.rs` (populate fields) |
| #10 | `src/domain/bundle.rs` | `src/agents/executor.rs` (populate locks_used) |
| #11 | `src/domain/lock.rs` | `src/daemon/handlers.rs` (accept ttl_secs) |
| #12 | `src/domain/tick.rs` | `src/agents/integrator_task.rs` (populate attempted) |
| #13 | `src/daemon/handlers.rs` | — |
| #14 | `src/daemon/handlers.rs` | (depends on #9) |
| #15 | `src/daemon/handlers.rs` | — |
| #16 | `src/daemon/handlers.rs` | `src/domain/work.rs` (add cycle detection fn) |
| #17 | `src/daemon/handlers.rs` | — |
| #18 | `src/daemon/handlers.rs` | — |
| #19 | `src/domain/bundle.rs`, `src/agents/executor.rs` | `src/daemon/handlers.rs` |
| #20 | `src/agents/mod.rs` | — |
| #21 | `src/agents/reviewer.rs` | — |
| #22 | `src/domain/proposal.rs` (new), `src/domain/decision.rs` (new) | `src/domain/mod.rs`, `src/daemon/context.rs`, `src/daemon/handlers.rs` |
| #23 | `src/tui/views/locks.rs` (new), `src/tui/app.rs` | `src/tui/views/mod.rs` |

### Phase 1: Critical Fixes (#1, #2, #3)

#### Defect #1: Worktree cleanup never called

**Root cause:** `run_agent_task()` in `executor.rs` moves `worktree_mgr` into the `AgentIpcBridge` at line 92, then never calls cleanup after the agent loop exits. Every Implementer/Reviewer run leaks an orphaned worktree directory and Git branch.

**Key observation:** `WorktreeManager` implements `Clone`. The call site in `handle_agent_start` (handlers.rs:2701) already clones it: `let task_worktree_mgr = worktree_mgr.clone()`. Within `run_agent_task`, the manager is used for `create()` and `worktree_path()` (lines 69-89), then moved into the bridge (line 92).

**Fix:**

1. Clone `worktree_mgr` before moving it into the bridge, keeping a copy for cleanup.
2. After the agent loop exits (success or failure), call `cleanup()` on the retained copy.
3. Insert the cleanup call after the match on `result`, before the terminal state transition.

```rust
// executor.rs — before bridge construction
let cleanup_mgr = worktree_mgr.clone();  // retain for cleanup
let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

// ... agent loop runs ...

// After agent loop exits, before terminal transition
if !agent_type.is_thinking_plane() {
    if let Err(e) = cleanup_mgr.cleanup() {
        warn!("Worktree cleanup failed for {}: {}", session_id, e);
        // Non-fatal — log and continue to terminal transition
    }
}
```

4. Also add cleanup to the daemon startup path so orphaned worktrees from previously killed agents are cleaned up on next start.

**Edge case — agent panic:** If the agent loop panics (rather than returning an error), the cleanup code after the agent loop won't execute. The daemon startup cleanup (step 4) handles this — on next daemon start, orphaned worktrees from crashed agents are detected and cleaned up. This is acceptable because panics should be rare and worktree accumulation between daemon restarts is bounded.

**Tests:**
- Unit test: After `run_agent_task()` completes for an Implementer, assert the worktree directory no longer exists on disk.
- Unit test: After `run_agent_task()` completes for a Coordinator (thinking plane), no cleanup is attempted.

#### Defect #2: Researcher previous_summary always None

**Root cause:** In `researcher.rs`, `let previous_summary: Option<String> = None;` is declared inside the `for iteration in 1..=config.max_iterations` loop body (line 406). It is never assigned the summary from the previous iteration's `IterationOutcome::Continue(summary)`.

**Fix:**

Move `previous_summary` declaration before the loop. Update the `Continue` match arm to assign the summary for the next iteration:

```rust
// researcher.rs
let mut previous_summary: Option<String> = None;

for iteration in 1..=config.max_iterations {
    // ... existing code ...

    let outcome = run_researcher_iteration(
        llm, session, stores, bridge, iteration, previous_summary.clone()
    ).await;

    match outcome {
        Ok(IterationOutcome::Continue(summary)) => {
            let _ = event_tx.send(DaemonEvent::agent_iteration_completed(
                &session.id, iteration, &summary
            ));
            info!("Researcher {} continue: {}", session.id, summary);
            previous_summary = Some(summary);  // <-- carry forward
        }
        Ok(IterationOutcome::Done(summary)) => {
            // ... existing terminal handling ...
            break;
        }
        // ... other arms unchanged ...
    }
}
```

**Tests:**
- Unit test: Call `run_researcher` with `max_iterations=3` using a mock LLM that returns `Continue(summary_N)` for iterations 1-2 and `Done` for iteration 3. Assert the context builder receives `previous_summary = Some("summary_2")` on iteration 3.

#### Defect #3: Integrator Tick fields not persisted to TaskStore

**Root cause:** `integrator_task.rs` lines 288-348 write `bundle_ids`, `validation_log`, and `integration_sha` directly to the in-memory `RwLock<HashMap>` but never flush through TaskStore's `store.lock().unwrap().update()`. On daemon restart, these fields are lost because TaskStore reloads from JSONL, which was never updated.

**Fix:**

After each in-memory field update, persist the tick through the unified TaskStore. The `Stores` struct has a single `store: Option<Arc<StdMutex<Store>>>` field that handles persistence for all record types. The pattern already exists in `handle_integrator_validate` and `handle_integrator_publish` handlers — replicate it:

```rust
// integrator_task.rs — after updating bundle_ids
// IMPORTANT: Clone tick data, then drop the write lock BEFORE acquiring the store lock.
// Holding both locks simultaneously would risk deadlock since handlers.rs
// acquires them in the opposite order (store lock → ticks write lock).
let tick_to_persist = {
    let mut ticks = stores.ticks.write().unwrap();
    if let Some(tick) = ticks.get_mut(&tick_id) {
        tick.bundle_ids = valid_bundle_ids.clone();
        Some(tick.clone())
    } else {
        None
    }
};  // write lock dropped here

if let Some(tick) = tick_to_persist {
    if let Some(ref store) = stores.store {
        if let Err(e) = store.lock().unwrap().update(tick) {
            warn!("Failed to persist tick bundle_ids: {}", e);
        }
    }
}
```

Apply the same clone-then-drop-then-persist pattern to `validation_log` and `integration_sha` updates. The field is `stores.store` (unified TaskStore), NOT a per-type store — Loopr uses a single `Store` instance for all record types.

**Lock ordering note:** Always drop the in-memory `RwLock` before acquiring the `Store` mutex. The handlers acquire them in the order `Store → ticks`, so the Integrator must not hold `ticks → Store` simultaneously.

Additionally, fix `handle_tick_transition` (handlers.rs line 1383) which is also missing the TaskStore persist call — add it to match the pattern in `handle_work_transition`.

**Tests:**
- Integration test: Create a Tick, update `bundle_ids` via the Integrator path, restart the daemon (reload from JSONL), assert `bundle_ids` is preserved.

### Phase 2: FSM & Handler Hardening (#4, #5, #6, #7, #8)

#### Defect #4: Work Integrated→Done missing Integrator role

**Root cause:** `work.rs` line 67-71 only allows `Role::Coordinator` for the Integrated→Done transition. The design says both Coordinator and Integrator should be able to close this.

**Fix:**

Add a second `TransitionRule` for Integrated→Done with `Role::Integrator`:

```rust
// work.rs — add to work_transitions()
TransitionRule {
    from: Integrated,
    to: Done,
    role: Some(Role::Coordinator),
},
TransitionRule {
    from: Integrated,
    to: Done,
    role: Some(Role::Integrator),
},
```

**Tests:**
- Unit test: `validate_transition(Integrated, Done, Role::Integrator, &rules)` succeeds.
- Existing test for `Role::Coordinator` continues to pass.

#### Defect #5: Tick Failed→Open transition missing

**Root cause:** `tick.rs` defines Failed as terminal — no outgoing transitions. The design conversation mentions Failed→Open for retry.

**Analysis:** The current Integrator approach treats Failed as terminal and simply creates a new Tick on the next cycle when Accepted bundles exist. This is functionally equivalent to Failed→Open→Sealing but avoids reusing a failed Tick's state (its `bundle_ids`, `validation_log`, `attempted_bundle_ids` are preserved as a historical record). The audit's "Not Defects" section even notes: "Failed→Open replaced by 'create new Tick' — reasonable alternative."

**Decision: Do NOT add Failed→Open.** The "create new Tick" approach is cleaner because:
1. The Failed Tick preserves its audit trail (which bundles were attempted, what validation failed).
2. Reusing a Failed Tick would require clearing `bundle_ids`, `validation_log`, and `attempted_bundle_ids`, losing the history.
3. The existing `test_invalid_failed_to_anything` test explicitly validates Failed as terminal.
4. The Integrator's `run_integrator_cycle` already handles this: if Accepted bundles exist and no Tick is in progress, it creates a fresh one.

**Fix:** Mark this as "intentional divergence — no code change needed." Update the design doc's FSM table to explicitly note that Failed is terminal and recovery is via new Tick creation, not state reset.

**Tests:** Existing test `test_invalid_failed_to_anything` already validates this behavior — no changes needed.

#### Defect #6: tick.create has no singleton guard

**Root cause:** `handle_tick_create` in handlers.rs creates a Tick without checking for existing non-terminal Ticks. The guard exists in `integrator_task.rs` (`has_tick_in_progress`) but not at the handler level.

**Fix:**

Add a guard at the top of `handle_tick_create`:

```rust
fn handle_tick_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    // Singleton guard: at most one non-terminal Tick at a time
    {
        let ticks = stores.ticks.read().unwrap();
        let active = ticks.values().any(|t| !t.status.is_terminal());
        if active {
            return DaemonResponse::err(
                req.id,
                RpcError::precondition_failed("A non-terminal Tick already exists"),
            );
        }
    }
    // ... existing creation logic ...
}
```

Add `is_terminal()` method to `TickStatus` (this method does NOT currently exist — only `AgentStatus` has `is_terminal()`):

```rust
impl TickStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, TickStatus::Published | TickStatus::Failed)
    }
}
```

**Note on interaction with #5:** Since #5 keeps Failed as terminal (no Failed→Open), `is_terminal()` correctly returns `true` for Failed. The singleton guard allows creating a new Tick when the only existing Tick is Failed (or Published).

**Tests:**
- Integration test: Create a Tick (Open), attempt to create another, assert error response with "non-terminal Tick already exists".
- Integration test: Create a Tick, transition to Published, create another, assert success.

#### Defect #7: Handshake never validates client version

**Root cause:** `handle_handshake` ignores `req.params` entirely. The client sends `client_version` but the handler never reads it.

**Fix:**

Read and compare client version against server version. Use semantic versioning major-version compatibility:

```rust
fn handle_handshake(req: DaemonRequest) -> DaemonResponse {
    let server_version = env!("CARGO_PKG_VERSION");
    let client_version = req.params
        .get("client_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Warn on version mismatch but don't reject — log and include in response
    let version_match = client_version == server_version;
    if !version_match {
        warn!(
            "Client version mismatch: client={}, server={}",
            client_version, server_version
        );
    }

    DaemonResponse::ok(
        req.id,
        json!({
            "server_version": server_version,
            "client_version": client_version,
            "version_match": version_match,
            "protocol": "ndjson/1"
        }),
    )
}
```

**Design choice:** Warn-only for now. Hard rejection would break rolling upgrades. The response includes `version_match: false` so clients can decide how to handle it.

**Tests:**
- Unit test: Handshake with matching version returns `version_match: true`.
- Unit test: Handshake with mismatched version returns `version_match: false` and still succeeds.
- Unit test: Handshake with no `client_version` returns `version_match: false` (graceful degradation).

#### Defect #8: No SIGKILL escalation on shutdown

**Root cause:** Daemon's `accept_loop` breaks on SIGINT/SIGTERM/IPC shutdown but never signals agent tasks to stop or waits for them.

**Fix:**

Add a graceful shutdown sequence to the daemon:

1. On shutdown signal, broadcast a `system.shutting_down` event (distinct from `system.shutdown`).
2. Cancel all running agent task `CancellationToken`s.
3. Wait up to `shutdown_grace_period_secs` (default: 10) for agent tasks to exit.
4. For any tasks still running after the grace period, abort the Tokio tasks (equivalent to SIGKILL for async tasks).
5. Clean up worktrees for any agents that didn't clean up after themselves.

```rust
// daemon/mod.rs — after breaking out of accept_loop
async fn graceful_shutdown(
    ctx: &Arc<RwLock<DaemonContext>>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    agent_handles: &mut Vec<JoinHandle<()>>,
    grace_period: Duration,
) {
    info!("Starting graceful shutdown, grace period: {:?}", grace_period);

    // 1. Broadcast shutting_down event
    let _ = event_tx.send(DaemonEvent::system_shutting_down());

    // 2. Cancel all agent sessions (existing cancellation mechanism)
    cancel_all_agent_sessions(ctx);

    // 3. Wait for tasks to finish, with timeout
    let deadline = tokio::time::sleep(grace_period);
    tokio::pin!(deadline);

    // Join all handles with timeout
    let result = tokio::time::timeout(
        grace_period,
        futures::future::join_all(agent_handles.drain(..))
    ).await;

    match result {
        Ok(_) => info!("All agent tasks exited gracefully"),
        Err(_) => {
            warn!("Grace period expired, aborting remaining tasks");
            for handle in agent_handles.drain(..) {
                handle.abort();
            }
        }
    }

    // 4. Cleanup orphaned worktrees
    cleanup_orphaned_worktrees(ctx);
}
```

**Prerequisites:**

1. **Store JoinHandles:** Add `agent_handles: StdMutex<HashMap<String, JoinHandle<()>>>` to `DaemonContext` (in `src/daemon/context.rs`). In `handle_agent_start` (handlers.rs:2698-2706), after `tokio::spawn`, insert the handle into this map keyed by session ID.

2. **cancel_all_agent_sessions:** Iterate `stores.agent_sessions`, transition each non-terminal session to `Cancelled` status. Agents already check for cancellation at the top of each iteration loop.

3. **cleanup_orphaned_worktrees:** List worktree directories under the configured worktree root. For any directory matching an agent session that no longer exists (or is in terminal state), call `WorktreeManager::cleanup()`.

**Tests:**
- Integration test: Start a daemon with an agent, send SIGTERM, assert agent task exits within grace period.
- Integration test: Start a daemon with a stuck agent (mock that never exits), send SIGTERM, assert task is aborted after grace period.

### Phase 3: Schema & Invariants (#9-#18)

#### Defect #9: Work missing acceptance_ref / checklist

**Fix:** Add fields to `Work` struct:

```rust
pub struct Work {
    // ... existing fields ...
    pub acceptance_criteria: Vec<String>,  // list of acceptance criteria
    pub checklist: Vec<ChecklistItem>,     // completion checklist
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecklistItem {
    pub description: String,
    pub completed: bool,
}
```

- Default to empty vecs for backward compatibility with existing JSONL data.
- `Work::new()` initializes both as empty.
- Migration: existing records load with empty vecs (serde default).

#### Defect #10: Bundle missing locks_used[]

**Fix:** Add field to `Bundle` struct:

```rust
pub struct Bundle {
    // ... existing fields ...
    pub locks_used: Vec<String>,  // IDs of Locks held during creation
}
```

- Default to empty vec.
- Implementer executor populates this from the bridge's lock state when proposing a Bundle.

#### Defect #11: Lock missing expires_at, renewable

**Fix:** Add fields to `Lock` struct:

```rust
pub struct Lock {
    // ... existing fields ...
    pub expires_at: Option<i64>,  // Unix millis, None = no expiry
    pub renewable: bool,          // default false
}
```

- Add a `is_expired(&self) -> bool` method that checks `expires_at` against current time.
- The Integrator loop (or a periodic task) checks for expired locks and calls `lock.expire` via IPC.
- `lock.acquire` IPC handler accepts optional `ttl_secs` param; computes `expires_at = now + ttl_secs * 1000`.

#### Defect #12: Tick missing attempted_bundle_ids[]

**Fix:** Add field to `Tick` struct:

```rust
pub struct Tick {
    // ... existing fields ...
    pub attempted_bundle_ids: Vec<String>,  // all bundles attempted (includes failures)
}
```

- When the Integrator seals a Tick, it sets `attempted_bundle_ids` to all candidate bundles.
- `bundle_ids` remains the subset that actually merged successfully.
- On validation failure, `attempted_bundle_ids` preserves the audit trail.

#### Defect #13: Work assignee set when InProgress/InReview

**Fix:** Add a post-transition invariant check in `handle_work_transition`:

```rust
// After successful FSM validation, before applying:
if matches!(target_status, WorkStatus::InProgress | WorkStatus::InReview) {
    if wi.assignee.is_none() {
        return DaemonResponse::err(
            req.id,
            RpcError::precondition_failed(
                "Work must have an assignee before transitioning to InProgress/InReview"
            ),
        );
    }
}
```

#### Defect #14: Work acceptance_criteria required for Ready

**Fix:** Add a pre-transition check for Draft→Ready:

```rust
if target_status == WorkStatus::Ready && wi.acceptance_criteria.is_empty() {
    return DaemonResponse::err(
        req.id,
        RpcError::precondition_failed(
            "Work must have acceptance_criteria before transitioning to Ready"
        ),
    );
}
```

This depends on Defect #9 being fixed first (field must exist).

#### Defect #15: Work InReview requires active Bundle

**Fix:** Add a cross-entity check in `handle_work_transition`:

```rust
if target_status == WorkStatus::InReview {
    let bundles = stores.bundles.read().unwrap();
    let has_active_bundle = bundles.values().any(|b| {
        b.work_id == wi.id
            && !matches!(
                b.status,
                BundleStatus::Rejected | BundleStatus::Merged | BundleStatus::Superseded
            )
    });
    if !has_active_bundle {
        return DaemonResponse::err(
            req.id,
            RpcError::precondition_failed(
                "Work cannot move to InReview without an active Bundle"
            ),
        );
    }
}
```

**Note:** `Superseded`, `Rejected`, and `Merged` bundles are all terminal/inactive — they don't count as "active."

#### Defect #16: Work depends_on acyclic

**Fix:** Add cycle detection in `handle_work_create` and a new `handle_work_update` (or in a shared validation function):

```rust
fn detect_dependency_cycle(
    works: &HashMap<String, Work>,
    start_id: &str,
    dependencies: &[String],
) -> bool {
    // BFS/DFS from each dependency to check if start_id is reachable
    let mut visited = HashSet::new();
    let mut queue: VecDeque<&str> = dependencies.iter().map(|s| s.as_str()).collect();

    while let Some(current) = queue.pop_front() {
        if current == start_id {
            return true; // cycle detected
        }
        if visited.insert(current) {
            if let Some(wi) = works.get(current) {
                for dep in &wi.dependencies {
                    queue.push_back(dep.as_str());
                }
            }
        }
    }
    false
}
```

Reject creation/update if cycle detected.

#### Defect #17: Work resource_tags non-empty

**Fix:** Validate in `handle_work_create`:

```rust
let resource_tags: Vec<String> = req.params
    .get("resource_tags")
    .and_then(|v| serde_json::from_value(v.clone()).ok())
    .unwrap_or_default();

if resource_tags.is_empty() {
    return DaemonResponse::err(
        req.id,
        RpcError::precondition_failed("Work must have at least one resource_tag"),
    );
}
```

#### Defect #18: Bundle at most one Accepted per Work

**Fix:** Add a uniqueness check in `handle_bundle_transition` when transitioning to Accepted:

```rust
if target_status == BundleStatus::Accepted {
    let bundles = stores.bundles.read().unwrap();
    let has_accepted = bundles.values().any(|b| {
        b.work_id == bundle.work_id
            && b.id != bundle.id
            && b.status == BundleStatus::Accepted
    });
    if has_accepted {
        return DaemonResponse::err(
            req.id,
            RpcError::precondition_failed(
                "Work already has an Accepted Bundle"
            ),
        );
    }
}
```

### Phase 4: Low-Priority Cleanup (#19-#23)

#### Defect #19: propose_bundle drops description

**Fix:**

1. Add `description: Option<String>` field to `Bundle` struct.
2. Pass `description` in the `bundle.create` IPC params from `executor.rs`.
3. Read and store it in `handle_bundle_create`.

```rust
// executor.rs — ProposeBundle arm
let resp = bridge.request(
    "bundle.create",
    serde_json::json!({
        "work_id": wi_id,
        "branch_name": branch_name,
        "claims": claims,
        "description": description,  // <-- add this
    }),
);
```

#### Defect #20: Reviewer creates unused worktree

**Fix:** Add `Reviewer` to the thinking plane check:

```rust
// agents/mod.rs or wherever is_thinking_plane() is defined
pub fn is_thinking_plane(&self) -> bool {
    matches!(
        self,
        AgentType::Coordinator
            | AgentType::Researcher
            | AgentType::Integrator
            | AgentType::Reviewer  // <-- add this
    )
}
```

This skips worktree creation for Reviewers. The Reviewer reads code context via the context builder (IPC calls to get file contents), never writes files.

#### Defect #21: Reviewer request_changes ≡ approve

**Root cause:** Both `Approve` and `RequestChanges` transition the Bundle to `Reviewed` (reviewer.rs lines 176-213). The distinction only appears in the Learning text. The Coordinator cannot tell from Bundle status whether changes were requested.

**FSM constraint:** The current Bundle FSM has no backward transition from `Triaged` to `Proposed`. The Reviewer receives bundles in `Triaged` status. Available Reviewer transitions are:
- `Triaged → Reviewed` (current behavior for both verdicts)
- `Triaged → Rejected` (available to Reviewer)

There is no `Triaged → Proposed` rule, and adding backward FSM transitions is architecturally undesirable (the FSM is designed to move forward).

**Fix:** Use `Rejected` for `RequestChanges`, with review feedback stored in a Learning. The Coordinator then decides whether to re-assign the Work to the Implementer (creating a new Bundle cycle):

```rust
// reviewer.rs
match review.verdict {
    ReviewVerdict::Approve => {
        bridge.request("bundle.transition", json!({
            "id": bundle_id,
            "target_status": "Reviewed",
            "role": "reviewer",
        }));
    }
    ReviewVerdict::RequestChanges => {
        // Reject the bundle — Coordinator will re-assign for rework
        bridge.request("bundle.transition", json!({
            "id": bundle_id,
            "target_status": "Rejected",
            "role": "reviewer",
        }));
        // Create a Learning with the review feedback so the next
        // Implementer iteration has context about what to fix
        bridge.request("learning.create", json!({
            "content": format!("Review requested changes: {}", review.feedback),
            "source": "reviewer",
            "work_id": work_id,
        }));
        // Note: Work transition back to InProgress happens via the
        // Coordinator's next iteration. The Coordinator sees a Rejected Bundle
        // with review feedback in the Learning, and decides whether to re-assign.
        // The Reviewer does NOT directly transition the Work — it lacks
        // Coordinator authority for that transition.
    }
    ReviewVerdict::Reject => {
        // ... existing Rejected logic (unchanged) ...
    }
}
```

**Why Rejected, not a new status:** The Bundle FSM already supports `Triaged → Rejected` for Reviewers. Adding a `ChangesRequested` status would require new FSM rules, new handler logic, and Coordinator awareness of a new state. Using `Rejected` + Learning keeps the FSM unchanged and leverages the existing feedback loop. The Learning record distinguishes "rejected (fatal)" from "rejected (rework requested)" via content.

**Tests:**
- Unit test: `RequestChanges` verdict transitions Bundle to Rejected (not Reviewed).
- Unit test: `RequestChanges` creates a Learning with review feedback.
- Unit test: `Approve` verdict still transitions Bundle to Reviewed.
- Update existing test `test_run_reviewer_request_changes` to expect Rejected status.

#### Defect #22: Proposal/Decision record types absent

**Fix:** Scaffold minimal stubs:

1. Create `src/domain/proposal.rs` and `src/domain/decision.rs` with struct definitions, status enums, and transition tables.
2. Register in `src/domain/mod.rs`.
3. Add TaskStore collections in `Stores`.
4. Add basic CRUD handlers (create, get, list).
5. Do NOT implement full agent integration — that's future work.

```rust
// domain/proposal.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProposalStatus {
    Draft,
    Open,
    Accepted,
    Rejected,
    Withdrawn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub title: String,
    pub description: String,
    pub author_id: String,
    pub status: ProposalStatus,
    pub created_at: i64,
    pub updated_at: i64,
}
```

```rust
// domain/decision.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DecisionStatus {
    Pending,
    Decided,
    Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub proposal_id: Option<String>,
    pub title: String,
    pub rationale: String,
    pub decided_by: String,
    pub status: DecisionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}
```

#### Defect #23: No TUI Locks view

**Fix:**

1. Add `Locks` variant to the `View` enum.
2. Update `View::ALL` array to include `Locks`.
3. Create `src/tui/views/locks.rs` following the pattern of existing views (e.g., `bundles.rs`).
4. Register in `src/tui/views/mod.rs`.
5. Wire into `current_list_len()`, `render_detail()`, and key handling.

The view should display: Lock ID, Resource, Holder, Status, Granted By, Created At, and (after #11) Expires At.

## Alternatives Considered

### Alternative 1: Fix only HIGH defects, defer everything else

- **Description:** Fix #1-#3 now, file issues for #4-#23.
- **Pros:** Minimal risk, fast.
- **Cons:** Missing invariants compound over time. The longer they're absent, the more invalid data accumulates and the harder the migration becomes. Fields like `acceptance_criteria` on Work need to be present before the Coordinator can properly generate Works.
- **Why not chosen:** The MEDIUM defects (#4-#12) affect agent behavior that is being actively developed. Deferring them creates a moving target.

### Alternative 2: Single monolithic PR

- **Description:** Fix all 23 defects in one commit/PR.
- **Pros:** One review cycle.
- **Cons:** Massive diff, hard to review, high regression risk, hard to bisect if something breaks.
- **Why not chosen:** Phased approach allows incremental validation.

### Alternative 3: Mandatory version rejection in handshake (#7)

- **Description:** Hard-reject clients with mismatched versions.
- **Pros:** Strict correctness.
- **Cons:** Breaks during development when daemon and CLI are at different build points. Forces lockstep rebuilds.
- **Why not chosen:** Warn-only with `version_match: false` in response is more practical for a local-only tool.

### Alternative 4: New ChangesRequested status for Bundle (#21)

- **Description:** Add a `ChangesRequested` variant to `BundleStatus` instead of using `Rejected`.
- **Pros:** More explicit state — distinguishes "fatal rejection" from "rework requested."
- **Cons:** Adds a new FSM state with its own transitions, complicates the Bundle lifecycle. The Coordinator and Implementer would need awareness of this new state. The FSM has no backward transitions by design.
- **Why not chosen:** Using `Rejected` + Learning preserves the forward-only FSM design. The Learning record captures the review feedback and the Implementer's next iteration receives it via the context builder.

### Alternative 5: Add Failed→Open for Tick retry (#5)

- **Description:** Allow Failed Ticks to be reopened instead of creating new Ticks.
- **Pros:** Explicit retry semantics.
- **Cons:** Requires clearing `bundle_ids`, `validation_log`, `attempted_bundle_ids` on the Failed Tick, losing the audit trail. The existing "create new Tick" approach preserves history.
- **Why not chosen:** The current approach (Failed Tick stays terminal, next cycle creates fresh Tick) preserves audit history and matches the Integrator's existing behavior.

## Technical Considerations

### Dependencies

- No new external crate dependencies required.
- Internal dependency: Defect #14 depends on #9 (field must exist before it can be validated).
- Internal dependency: Defect #13 requires Work assignee to be settable, which it already is via existing IPC.

### Performance

- Cycle detection (#16) is O(V+E) on the Work dependency graph. With typical project sizes (< 1000 Works), this is negligible.
- Singleton guard (#6) scans all Ticks on each create call. With typical Tick counts (< 100 per project), this is negligible.
- Bundle uniqueness check (#18) scans all Bundles for a Work. Typical count is < 10 per Work.

### Security

- No new attack surfaces. All fixes operate within the existing IPC trust boundary.
- Researcher path sandboxing is unaffected.

### Testing Strategy

Every defect fix includes targeted tests:

| Phase | Test Type | Count |
|-------|-----------|-------|
| P1 | Unit + integration (mock LLM, tmpdir worktrees) | ~6 |
| P2 | Unit (FSM validation) + integration (handler responses) | ~10 |
| P3 | Unit (invariant checks) + integration (rejection responses) | ~12 |
| P4 | Unit (struct fields) + integration (TUI view rendering) | ~8 |

All tests run under `otto ci` (lint + check + test).

### Rollout Plan

1. Each phase is a separate commit (or small group of commits).
2. `otto ci` must pass after each phase before proceeding.
3. Phase ordering enforces dependencies (P3 #14 depends on P3 #9, but both are in P3 so #9 lands first within that phase).
4. Schema changes (#9-#12) use serde defaults for backward compatibility — existing JSONL files load without migration.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Schema changes break JSONL deserialization | Low | High | All new fields use `#[serde(default)]`. Test with existing JSONL fixtures. |
| Invariant enforcement breaks existing tests | Medium | Low | Run `otto ci` after each fix. Existing tests may need updated fixtures (e.g., Works now need resource_tags). |
| Reviewer request_changes→Rejected creates rework loop | Low | Medium | Coordinator tracks Bundle rejection count per Work. After N rejections (configurable, default 3), Coordinator escalates to NeedHelp instead of re-assigning. |
| Graceful shutdown (#8) adds complexity to daemon lifecycle | Medium | Medium | Feature-flag behind config. Default enabled but can be disabled for debugging. |
| Cycle detection (#16) false positives on valid DAGs | Low | Low | Standard BFS — well-understood algorithm. Unit tests cover diamond dependencies. |
| #3 fix introduces deadlock if lock ordering violated | Low | High | Clone-then-drop-then-persist pattern. Document lock ordering rule: never hold in-memory RwLock while acquiring Store mutex. |
| Existing tests break due to new required fields (#9, #17) | High | Low | Use `#[serde(default)]` on all new fields. Update test fixtures that create Works to include required fields. |

## Open Questions

- [x] ~~Should the Tick singleton guard (#6) allow creating a new Open Tick while a Failed one exists?~~ **Resolved:** Yes. #5 keeps Failed as terminal (no Failed→Open), so `is_terminal()` returns true for Failed. No conflict.
- [x] ~~For #21 (request_changes), should we reuse Proposed or add a dedicated ChangesRequested status?~~ **Resolved:** Use Rejected + Learning. The existing FSM has no backward transitions. Rejected + Learning + Work re-assignment is the cleanest path.
- [ ] For #22 (Proposal/Decision), how much of the struct should be scaffolded now vs. designed properly later? (Current fix: minimal stubs with CRUD handlers, no agent integration.)
- [ ] Should #8 (graceful shutdown) store JoinHandles in `DaemonContext` or in `Stores`? `DaemonContext` is behind `Arc<RwLock<>>` and is the natural home for runtime state. `Stores` is for domain data. Recommend `DaemonContext`.

## Defect Coverage Summary

All 23 defects from the audit are addressed:

| # | Defect | Phase | Action |
|---|--------|-------|--------|
| 1 | Worktree cleanup never called | P1 | Clone mgr, cleanup after agent loop |
| 2 | Researcher previous_summary always None | P1 | Move declaration before loop, carry forward |
| 3 | Integrator Tick fields not persisted | P1 | Clone-drop-persist pattern to TaskStore |
| 4 | Work Integrated→Done missing Integrator | P2 | Add TransitionRule for Role::Integrator |
| 5 | Tick Failed→Open missing | P2 | No code change — intentional divergence (create new Tick) |
| 6 | tick.create no singleton guard | P2 | Add is_terminal() + guard in handler |
| 7 | Handshake ignores client version | P2 | Read, compare, warn, include in response |
| 8 | No SIGKILL escalation on shutdown | P2 | Store JoinHandles, graceful shutdown sequence |
| 9 | Work missing acceptance_criteria/checklist | P3 | Add fields with #[serde(default)] |
| 10 | Bundle missing locks_used[] | P3 | Add field, populate from executor |
| 11 | Lock missing expires_at, renewable | P3 | Add fields + is_expired() method |
| 12 | Tick missing attempted_bundle_ids[] | P3 | Add field, populate at seal time |
| 13 | Work assignee required for InProgress/InReview | P3 | Pre-transition check in handler |
| 14 | Work acceptance_criteria required for Ready | P3 | Pre-transition check (depends on #9) |
| 15 | Work InReview requires active Bundle | P3 | Cross-entity check in handler |
| 16 | Work depends_on acyclic | P3 | BFS cycle detection |
| 17 | Work resource_tags non-empty | P3 | Validate on create |
| 18 | Bundle at most one Accepted per Work | P3 | Uniqueness check on transition |
| 19 | propose_bundle drops description | P4 | Add description field to Bundle, pass through |
| 20 | Reviewer creates unused worktree | P4 | Add Reviewer to is_thinking_plane() |
| 21 | Reviewer request_changes ≡ approve | P4 | Use Rejected + Learning for RequestChanges |
| 22 | Proposal/Decision types absent | P4 | Scaffold stubs with CRUD handlers |
| 23 | No TUI Locks view | P4 | Add Locks variant + view module |

## References

- `docs/design/2026-02-26-loopr-v3-mvp4.md` — MVP4 design doc (source of truth)
- `docs/design/2026-02-26-loopr-v3-mvp3.md` — MVP3 design doc
- `docs/design/2026-02-25-loopr-v3-mvp1.md` — MVP1 design doc
- Audit conversation (this session) — defect discovery and triage
