# Design Document: Pipeline Hardening - Locks & Timeouts

**Author:** Scott A. Idler
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

This document covers three low-level orchestration safety mechanisms identified during earlier audits (2026-02-27) that remain unimplemented: (1) auto-acquiring advisory locks before file modifications, (2) enforcing wall-clock session timeouts on agent tasks, and (3) detecting circular dependencies in Work items. The codebase has evolved significantly since the original designs - the lock domain model, lane architecture, and dependency display are all in place - making these final gaps straightforward to close.

## Problem Statement

### Background

Loopr isolates agents in git worktrees and bounds their work via iteration budgets (`max_iterations`) and per-tool timeouts (lane architecture with semaphore slots). These mechanisms cover the common case well. However, three gaps remain that could cause silent data corruption, runaway sessions, or coordinator deadlocks under adversarial or degenerate conditions.

The original audit (2026-02-27-audit-fixes.md) flagged these as Defects #14, #15, and #16. Since then, the Runner Lane Architecture (next-steps #2) was completed, adding robust tool-level timeouts and process-group isolation. The advisory lock domain model (`src/domain/lock.rs`) and all lock handlers were implemented. The Coordinator now displays dependency satisfaction status to the LLM. What remains is wiring the last mile: auto-lock acquisition, session-level timeout enforcement, and cycle detection validation.

### Problem

1. **File modification without advisory locks.** `WriteFile` and `EditFile` actions in `src/agents/executor.rs` write files without acquiring advisory locks. The lock infrastructure exists (domain model, handlers, config, executor check for `LockStrict`) but is never triggered. The Coordinator's state summary has code to display active locks, but it always shows empty because no locks are ever created. This means file contention between agents is invisible until merge time.

2. **No wall-clock session timeout.** `session_timeout_secs` is defined in `AgentRoleConfig` with sensible defaults (Implementer: 1800s, Reviewer: 600s, Researcher: 600s, Integrator: 1200s) but is never read or enforced. While `max_iterations` prevents infinite LLM loops, and lane timeouts prevent individual tool hangs, nothing prevents the aggregate session from running indefinitely - for example, an agent that makes 20 short LLM calls but each tool execution takes minutes would stay within iteration budget while exceeding any reasonable wall-clock limit.

3. **No circular dependency detection.** Work items support a `dependencies: Vec<String>` field. Creation validates that referenced IDs exist (filtering unknowns with a warning), and the Coordinator displays READY/BLOCKED labels to the LLM. But there is no check for cycles (A depends on B depends on A) or self-references. A cycle would cause the Coordinator to deadlock - neither work item would ever become Ready, and the LLM would see both as permanently BLOCKED.

### Goals

- Auto-acquire advisory locks on every `WriteFile` and `EditFile` action; release all locks held by an agent on exit (including panic/timeout paths)
- Enforce `session_timeout_secs` as a hard wall-clock bound on `run_agent_task()` via `tokio::time::timeout`
- Reject Work items with circular or self-referencing dependencies at creation time via BFS cycle detection

### Non-Goals

- **Kernel-level file locking (flock/fcntl).** Advisory locks are application-level, persisted in TaskStore. OS-level locks add complexity without benefit since all file access goes through the executor.
- **ReadFile locking.** Reads are non-destructive. Advisory locks are write-only.
- **Hard dependency enforcement at transition time.** The Coordinator treats dependency status as guidance, not a gate. This is intentional - it allows the LLM to override when appropriate (e.g., unblocking a work item manually). Cycle detection at creation time is sufficient to prevent deadlocks.
- **Memory/CPU resource limits on agents.** Out of scope for this work; could be a future lane-level enhancement.
- **Timeout on the Coordinator.** The Coordinator is long-lived by design (`session_timeout_secs: None`). Its lifecycle is managed by the Supervisor with restart/backoff logic.

## Proposed Solution

### Overview

Three independent changes, each scoped to a single file or narrow call site:

1. **Auto-lock on write** - Two insertion points in `src/agents/executor.rs` (WriteFile handler ~line 480, EditFile handler ~line 515), plus cleanup logic near agent exit (~line 263).
2. **Session timeout** - One insertion point wrapping the `run_agent_loop()` call inside `run_agent_task()` (~line 236) with `tokio::time::timeout`.
3. **Cycle detection** - One validation function added to `src/daemon/handlers.rs` at work creation (~line 1437).

### Component 1: Auto-Lock on File Modifications

#### Current Flow (WriteFile)

```
validate_sandboxed_path() -> [LockStrict check: reject if ANY lock exists] -> tokio::fs::write()
```

#### Proposed Flow (WriteFile)

```
validate_sandboxed_path() -> auto_acquire_lock() -> [LockStrict check: reject if OTHER agent holds lock] -> tokio::fs::write()
```

#### Prerequisite Fix: LockStrict Self-Blocking Bug

The existing `LockStrict` check (`executor.rs:485-498`) rejects writes if **any** active lock exists on the resource, without checking the holder. With auto-locks, this would cause self-blocking - an agent that auto-acquired a lock on its first write would be rejected on its second write to the same file.

**Current (broken with auto-locks):**
```rust
if let Some(locks) = lock_resp.result.as_ref().and_then(|v| v.as_array())
    && !locks.is_empty()
{
    let holder = locks[0].get("holder_id")...;
    return Ok(ActionResult::ActionError(...));
}
```

**Fixed:**
```rust
if let Some(locks) = lock_resp.result.as_ref().and_then(|v| v.as_array()) {
    let dominated_by_other = locks.iter().any(|l| {
        l.get("holder_id").and_then(|v| v.as_str()) != work_id
    });
    if dominated_by_other {
        let holder = locks[0].get("holder_id")...;
        return Ok(ActionResult::ActionError(...));
    }
}
```

This fix is required regardless of auto-locks (the existing `AcquireLock` action has the same self-blocking problem), but becomes critical once auto-locks populate the lock store.

#### Auto-Lock Acquisition

Before any file write, the executor acquires an advisory lock for the current work item:

```rust
// In execute_action(), before the LockStrict check:
fn auto_acquire_write_lock(
    bridge: &AgentIpcBridge,
    resource: &str,
    holder_id: &str,
) -> Option<String> {
    // Check if we already hold a lock on this resource
    let check = bridge.request(
        "lock.list",
        json!({ "resource": resource, "holder_id": holder_id, "active_only": true }),
    );
    let already_held = check.result
        .as_ref()
        .and_then(|v| v.as_array())
        .is_some_and(|locks| !locks.is_empty());

    if already_held {
        return None; // Already locked by us, no new lock needed
    }

    // Check if another agent already holds a lock (advisory warning)
    let existing = bridge.request(
        "lock.list",
        json!({ "resource": resource, "active_only": true }),
    );
    if let Some(locks) = existing.result.as_ref().and_then(|v| v.as_array()) {
        for lock in locks {
            if let Some(other) = lock.get("holder_id").and_then(|v| v.as_str()) {
                if other != holder_id {
                    log::warn!(
                        "advisory lock contention: {} already holds lock on {}, acquiring concurrent lock for {}",
                        other, resource, holder_id
                    );
                }
            }
        }
    }

    // Acquire new lock (best-effort under LockAdvisory)
    let resp = bridge.request(
        "lock.create",
        json!({
            "resource": resource,
            "holder_id": holder_id,
            "granted_by": holder_id,
        }),
    );
    resp.result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|id| id.as_str())
        .map(String::from)
}
```

This function is called in both the `WriteFile` and `EditFile` arms of `execute_action()`. The `holder_id` is the Work item ID (e.g., `wi-abc123`), available via the `work_id: Option<&str>` parameter already passed to `execute_action()`. When `work_id` is `None` (thinking-plane agents that don't write files through the executor), auto-lock is skipped.

#### Lock Cleanup on Agent Exit

At agent exit (both normal and error paths), release all locks held by the agent's work ID. This runs in `run_agent_task()` after `run_agent_loop()` returns and before worktree cleanup (~line 263). The `bridge` is still alive at this point (it was borrowed by reference into the loop, not moved).

The `holder_id` for cleanup must match what was used during auto-acquire: the `work_id` from the session. Read it from the session store, not from `worktree_key` (which is `bundle_id` for Reviewers).

```rust
// In run_agent_task(), between run_agent_loop() result and worktree cleanup:
fn release_agent_locks(bridge: &AgentIpcBridge, holder_id: &str, agent_log: &AgentLogger) {
    let resp = bridge.request(
        "lock.list",
        json!({ "holder_id": holder_id, "active_only": true }),
    );
    if let Some(locks) = resp.result.as_ref().and_then(|v| v.as_array()) {
        for lock in locks {
            if let Some(lock_id) = lock.get("id").and_then(|v| v.as_str()) {
                let _ = bridge.request("lock.release", json!({ "id": lock_id }));
            }
        }
        if !locks.is_empty() {
            agent_log.info(&format!("released {} advisory lock(s)", locks.len()));
        }
    }
}

// Called as:
if let Ok(sessions) = stores.read_agent_sessions() {
    if let Some(session) = sessions.get(&session_id) {
        if let Some(ref wi_id) = session.work_id {
            release_agent_locks(&bridge, wi_id, &agent_log);
        }
    }
}
```

The existing `handle_lock_create` handler also auto-expires stale locks (using `max_lock_ttl_minutes`, default 60 min) as a backstop for cases where cleanup doesn't run (e.g., daemon crash).

#### Interaction with Existing Lock Infrastructure

| Component | Current State | After This Change |
|-----------|--------------|-------------------|
| `Lock` domain model | Complete | No changes needed |
| Lock handlers (create/list/release/expire) | Complete | No changes needed |
| `ConflictPolicy::LockStrict` executor check | Implemented but self-blocking bug | Fixed to exclude holder, now effective with auto-locks |
| Coordinator state summary lock display | Coded but empty | Now populated with real lock data |
| `max_lock_ttl_minutes` config | Set to 60, used by handler | Acts as backstop for missed cleanups |

### Component 2: Session-Level Wall-Clock Timeout

#### Current Flow

```rust
// In run_agent_task():
let result = run_agent_loop(/* ... */).await;
// result processed, cleanup runs
```

#### Proposed Flow

```rust
// In run_agent_task(), resolve timeout from the correct config struct:
let timeout_secs = match agent_type {
    AgentType::Implementer => stores.config.agents.implementer.session_timeout_secs,
    AgentType::Reviewer => stores.config.agents.reviewer.session_timeout_secs,
    AgentType::Researcher => stores.config.agents.researcher.session_timeout_secs,
    AgentType::Coordinator => stores.config.agents.coordinator.role.session_timeout_secs,
    AgentType::Integrator => stores.config.integrator.session_timeout_secs,
    AgentType::Chat => None, // Chat sessions managed separately
};

let loop_future = run_agent_loop(&session_id, agent_type, &stores, &bridge, &event_tx, &agent_log);

let result = if let Some(secs) = timeout_secs {
    match tokio::time::timeout(Duration::from_secs(secs), loop_future).await {
        Ok(inner) => inner,
        Err(_elapsed) => {
            agent_log.warn(&format!("session timeout after {}s", secs));
            Err(eyre::eyre!("session timed out after {}s", secs))
        }
    }
} else {
    // No timeout (Coordinator case)
    loop_future.await
};
// Cleanup runs normally (lock release, worktree cleanup, status transition)
```

Note: `session_timeout_secs` lives in different config structs per agent type. Implementer/Reviewer/Researcher use `AgentRoleConfig`, Coordinator wraps it in `CoordinatorConfig.role`, and Integrator has its own `IntegratorConfig`. The match above resolves this at the call site.

#### Why This Works

- The cleanup path in `run_agent_task()` already handles both `Ok` and `Err` results from the agent loop. A timeout error flows through the same `Err` path, triggering lock cleanup, worktree cleanup, and a transition to `AgentStatus::Failed`.
- The Coordinator has `session_timeout_secs: None`, so it is unaffected.
- Tool-level timeouts (via lanes) remain the first line of defense for individual operations. The session timeout is a coarser safety net for the aggregate.

#### Timeout Hierarchy (After This Change)

| Layer | Mechanism | Scope | Defaults |
|-------|-----------|-------|----------|
| Tool execution | `spawn_with_process_group()` with SIGTERM/SIGKILL | Single subprocess | Per-tool (30-300s) |
| Lane policy | `LanePolicy.max_timeout_secs` | Tool class | Local=60s, Net=120s, Heavy=1800s |
| Iteration budget | `max_iterations` loop bound | LLM turn count | Impl=20, Rev=5, Res=10 |
| **Session timeout** | **`tokio::time::timeout` on `run_agent_loop()`** | **Entire agent session** | **Impl=1800s, Rev=600s, Res=600s, Int=1200s** |
| Daemon shutdown | Grace period timeout | All agents | 30s |

### Component 3: Acyclic Dependency Validation

#### Detection Algorithm

BFS from each declared dependency, checking if the new work item's ID is reachable:

```rust
fn detect_dependency_cycle(
    works: &HashMap<String, Work>,
    new_id: &str,
    dependencies: &[String],
) -> bool {
    let mut visited = HashSet::new();
    let mut queue: VecDeque<&str> = dependencies.iter().map(|s| s.as_str()).collect();

    while let Some(current) = queue.pop_front() {
        if current == new_id {
            return true; // Cycle detected
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

#### Integration Point: `handle_work_create()`

In `handle_work_create()` (`src/daemon/handlers.rs`), the current flow is:

1. Lines 1431-1457: Parse and validate dependency IDs (filter unknowns)
2. Line 1459: `Work::new()` generates the work ID
3. Line 1462: `work.dependencies = dependencies`
4. Lines 1474-1488: Persist to TaskStore and in-memory store

The cycle check goes between steps 3 and 4. The `works` map is already available from the dependency validation block (line 1439). The new work's ID comes from `work.id`:

```rust
// After work.dependencies = dependencies (line 1462):
if !work.dependencies.is_empty() {
    let works = stores.read_works()?;
    if detect_dependency_cycle(&works, &work.id, &work.dependencies) {
        return Ok(DaemonResponse::err(
            req.id,
            RpcError::precondition_failed(
                "Circular dependency detected: adding these dependencies would create a cycle",
            ),
        ));
    }
}
```

#### Integration Point: `handle_work_update()`

The `handle_work_update` RPC handler (~line 5350 in `src/daemon/handlers.rs`) allows clients to modify the `dependencies` array on existing Work items via `wi.dependencies = deps`. Without cycle detection here, the Coordinator could create Work A (no deps), create Work B depending on A, then update A to depend on B - creating a cycle that bypasses the creation-time gate.

The same `detect_dependency_cycle()` function applies. The only difference is the work already exists in the store, so it must be temporarily excluded from the map to avoid a false positive (the work's old dependencies would be in the BFS graph):

```rust
// In handle_work_update(), when dependencies are modified:
if let Some(ref new_deps) = updated_deps {
    if !new_deps.is_empty() {
        let mut works = stores.read_works()?;
        works.remove(&work_id); // Exclude self to avoid stale edges
        if detect_dependency_cycle(&works, &work_id, new_deps) {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::precondition_failed(
                    "Circular dependency detected: updating dependencies would create a cycle",
                ),
            ));
        }
    }
}
```

#### Self-Reference

Self-references (a work depending on itself) are a degenerate cycle caught by the same BFS: if `new_id` appears in `dependencies`, the first iteration finds it.

#### Batch Dependency Resolution

Batch references (`batch:0`, `batch:1`) are resolved to real IDs by `resolve_batch_dependencies()` in the Coordinator before the `CreateWork` RPC reaches the handler. So cycle detection sees real IDs, not batch refs.

#### Existing Dependency Infrastructure (No Changes Needed)

| Component | Status |
|-----------|--------|
| `dependencies: Vec<String>` field on Work | Complete |
| ID existence validation at creation | Complete |
| Batch reference resolution (`batch:N`) | Complete |
| Tag-based dependency pruning | Complete |
| Coordinator READY/BLOCKED display | Complete |
| Assignee/acceptance_criteria invariants | Complete |

## Alternatives Considered

### Alternative 1: OS-Level File Locking (flock/fcntl)

- **Description:** Use kernel-level file locks via the `fs2` crate instead of application-level advisory locks.
- **Pros:** Kernel-enforced, survives process crashes, works across unrelated processes.
- **Cons:** Doesn't survive daemon crashes (locks tied to file descriptors). Requires holding file descriptors open. Doesn't integrate with TaskStore persistence or Coordinator visibility. Adds external dependency.
- **Why not chosen:** All file access goes through the executor, so application-level locks provide identical guarantees with better visibility and simpler implementation.

### Alternative 2: Hard Dependency Enforcement at Transition Time

- **Description:** Reject `Ready -> InProgress` transitions if any dependency is not `Done`, enforced in `handle_work_transition()`.
- **Pros:** Guarantees ordering, prevents LLM mistakes.
- **Cons:** Removes flexibility. The Coordinator LLM may have legitimate reasons to override (e.g., partial dependency satisfaction is sufficient, or manually unblocking). Makes the system more brittle.
- **Why not chosen:** The current design philosophy treats dependencies as guidance for the Coordinator LLM, not hard gates. Cycle detection at creation time prevents the deadlock scenario; enforcement beyond that is better left to the LLM's judgment.

### Alternative 3: Per-Iteration Timeout Instead of Per-Session

- **Description:** Wrap each individual LLM call + tool execution iteration in a timeout rather than the whole session.
- **Pros:** Finer-grained control, catches stuck individual iterations.
- **Cons:** Doesn't prevent a session of many fast iterations from running too long in aggregate. Tool-level timeouts already cover the "stuck tool" case via lanes.
- **Why not chosen:** Tool-level timeouts already handle individual operation hangs. The session timeout addresses the aggregate case that iteration timeouts miss.

### Alternative 4: Topological Sort Instead of BFS Cycle Detection

- **Description:** Use Kahn's algorithm (topological sort) to detect cycles across all Work items whenever a new one is created.
- **Pros:** Detects cycles globally, produces a valid execution order.
- **Cons:** O(V+E) over all work items on every creation, unnecessary since we only need to check if the new item introduces a cycle. BFS from the new item's dependencies is O(reachable nodes), typically much smaller.
- **Why not chosen:** BFS from the new item's declared dependencies is cheaper and sufficient. A full topological sort would be appropriate if we ever need to enforce execution ordering system-wide.

## Technical Considerations

### Dependencies

- **No new crate dependencies.** All three changes use existing infrastructure (IPC bridge, `tokio::time::timeout`, `std::collections::{HashSet, VecDeque}`).
- **Internal dependencies:** Lock domain model, lock handlers, IPC bridge, `AgentRoleConfig` - all already implemented and stable.

### Performance

- **Auto-lock acquisition:** Two IPC round-trips per file write (list + create). These are in-process function calls routed through the daemon dispatcher, not network calls. Negligible latency compared to LLM API calls or tool subprocess execution.
- **Lock cleanup:** One IPC call (list) + N release calls at agent exit. Typically N < 20 files per agent session. Runs once at exit, not in the hot path.
- **Session timeout:** Zero overhead - `tokio::time::timeout` is a lightweight future wrapper.
- **Cycle detection:** BFS is O(V+E) where V = reachable works from dependencies. In practice, dependency chains are short (2-5 items). Runs once at work creation, not in a loop.

### Security

- Advisory locks are application-level and scoped to the daemon process. They don't affect the host filesystem or other processes.
- Session timeouts prevent resource exhaustion from runaway agents consuming API credits or compute.
- Cycle detection prevents a denial-of-service vector where a malformed LLM response could deadlock the Coordinator.

### Testing Strategy

#### Component 1: Auto-Lock Tests

```rust
#[test]
fn test_write_file_auto_acquires_lock() {
    // Execute WriteFile action with a work_id
    // Verify lock.list shows an active lock for that resource + holder
}

#[test]
fn test_edit_file_auto_acquires_lock() {
    // Execute EditFile action with a work_id
    // Verify lock.list shows an active lock for that resource + holder
}

#[test]
fn test_write_file_reuses_existing_lock() {
    // Write same file twice with same work_id
    // Verify only one lock exists (no duplicates)
}

#[test]
fn test_agent_exit_releases_locks() {
    // Execute multiple writes, then simulate agent exit
    // Verify all locks are Released
}

#[test]
fn test_lock_strict_allows_holder_rewrite() {
    // Agent A writes file (auto-acquires lock), writes same file again
    // Verify second write succeeds (holder is not self-blocked)
}

#[test]
fn test_lock_strict_blocks_other_agent() {
    // Agent A writes file (auto-acquires lock)
    // Agent B attempts to write same file under LockStrict
    // Verify Agent B's write is rejected with holder info
}

#[test]
fn test_lock_advisory_allows_concurrent_with_warning() {
    // Agent A writes file (auto-acquires lock)
    // Agent B writes same file under LockAdvisory
    // Verify both writes succeed (advisory is non-blocking)
    // Verify both locks visible in lock.list
}
```

#### Component 2: Session Timeout Tests

```rust
#[test]
fn test_session_timeout_terminates_agent() {
    // Configure session_timeout_secs = 1 (very short)
    // Run agent with a slow mock LLM
    // Verify agent transitions to Failed with timeout message
}

#[test]
fn test_session_timeout_none_allows_indefinite() {
    // Configure session_timeout_secs = None (Coordinator case)
    // Verify no timeout wrapper is applied
}

#[test]
fn test_session_timeout_cleanup_runs() {
    // Timeout an agent that has acquired locks + worktree
    // Verify locks released and worktree cleaned up
}
```

#### Component 3: Cycle Detection Tests

```rust
#[test]
fn test_self_referencing_dependency_rejected() {
    // Create work with dependency on itself
    // Verify precondition_failed error
}

#[test]
fn test_direct_cycle_rejected_at_creation() {
    // Create A, then attempt to create B depending on A with A depending on B
    // Verify cycle detected and rejected
}

#[test]
fn test_direct_cycle_rejected_at_update() {
    // Create A, then B depending on A
    // Update A to depend on B
    // Verify cycle detected and rejected via handle_work_update
}

#[test]
fn test_transitive_cycle_rejected() {
    // Create A -> B -> C, then D depending on A with C depending on D
    // Verify cycle detected
}

#[test]
fn test_valid_chain_accepted() {
    // Create A -> B -> C (linear chain)
    // Verify all accepted without error
}

#[test]
fn test_diamond_dependency_accepted() {
    // A -> B, A -> C, B -> D, C -> D (diamond, no cycle)
    // Verify accepted
}
```

### Implementation Plan

All three components are independent and can be implemented in parallel. Each is a targeted insertion into existing code with no schema or API changes.

**Phase 1: Auto-Lock on File Modifications**
- First: write `test_lock_strict_allows_holder_rewrite` test, verify it fails against current self-blocking code (TDD the prerequisite fix)
- Apply the `LockStrict` self-blocking fix, verify the test passes
- Add `auto_acquire_write_lock()` helper to `src/agents/executor.rs` (with `log::warn!` for advisory contention)
- Insert calls in `WriteFile` and `EditFile` action handlers
- Add `release_agent_locks()` helper
- Insert call in `run_agent_task()` cleanup path (before worktree cleanup)
- Add remaining unit tests

**Phase 2: Session-Level Timeout**
- Read `session_timeout_secs` from the agent's role config in `run_agent_task()`
- Wrap `run_agent_loop()` call in conditional `tokio::time::timeout`
- Verify cleanup path handles timeout errors correctly
- Add unit tests

**Phase 3: Cycle Detection**
- Add `detect_dependency_cycle()` function to `src/daemon/handlers.rs` (or a shared validation module)
- Insert call in `handle_work_create()` after dependency existence validation
- Insert call in `handle_work_update()` when dependencies are modified (with `works.remove(&work_id)` to exclude stale self-edges)
- Return `precondition_failed` error on cycle detection
- Add unit tests (including update-path cycle test)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Auto-lock IPC calls slow down file writes | Low | Low | IPC is in-process dispatch, not network. Benchmark if concerned. |
| Session timeout kills agent mid-write | Low | Medium | Lock cleanup + worktree cleanup already handle abrupt exit. Writes are to agent-isolated worktrees, so partial writes don't corrupt the main branch. The Integrator validates before merging. |
| Cycle detection rejects valid batch dependencies | Low | Medium | Batch refs are resolved to real IDs before handler sees them. Test with batch scenarios. |
| Lock cleanup doesn't run on daemon crash | Medium | Low | `max_lock_ttl_minutes` (60 min) auto-expires stale locks. Handler calls `expire_stale_locks()` before creating new locks. |
| False positive cycle detection from stale works | Low | Low | BFS only traverses works that exist in the store. Abandoned/Done works still have their dependency data, which is correct for cycle detection purposes. |
| Session timeout drops future with in-flight subprocess | Low | Medium | Tool subprocesses run in their own process group (`setsid()`). When the tokio future is dropped, the process group is orphaned. The lane semaphore permit is also dropped, but the subprocess continues until its own lane timeout fires. Edge case: if the lane timeout exceeds the session timeout (e.g., Heavy lane at 1800s = Implementer session timeout), the subprocess can outlive the session. In practice this is bounded by daemon shutdown cleanup. If tighter guarantees are needed, the session timeout path could explicitly kill the process group for any in-flight tool before returning. |
| Concurrent batch creations bypass cycle detection | Low | Low | Two concurrent `CreateWork` batches could theoretically create an interlocking cycle (A depends on D from batch 2, D depends on A from batch 1) because each check sees only its own batch. Mitigated by: (1) single Coordinator means batches are sequential in practice, (2) the Coordinator LLM is unlikely to create cross-batch cycles. If this becomes a concern, a global re-validation pass after batch creation would catch it. |
| Auto-lock with `work_id: None` | Low | None | Thinking-plane agents (Coordinator, Researcher) have `work_id: None` in `execute_action`. Auto-lock is skipped. These agents don't write files through the executor - they use IPC for state changes. No action needed. |

## Resolved Questions

- [x] **Advisory contention warning:** Yes. `auto_acquire_write_lock` now logs `log::warn!` when another agent holds a lock on the same file under `LockAdvisory`. The lock is still acquired (advisory is non-blocking), but the warning gives visibility into contention in agent logs for post-mortem debugging. Code updated in Component 1.
- [x] **Cycle detection on `work.update`:** Yes. `handle_work_update` allows dependency modification (~line 5350), so a cycle could be introduced after creation (e.g., create A, create B->A, update A->B). The same `detect_dependency_cycle()` is now called in both `handle_work_create` and `handle_work_update`. See Component 3 for the update-path integration.
- [x] **TDD order for self-blocking fix:** Yes. Write the `test_lock_strict_allows_holder_rewrite` test first, verify it fails against the current code, apply the fix, verify it passes - then proceed with auto-lock implementation. Phase 1 plan updated to reflect this order.

## References

- [2026-03-01-file-touch-broadcasting.md](2026-03-01-file-touch-broadcasting.md) - original file-touch advisory lock design
- [2026-02-27-audit-fixes.md](2026-02-27-audit-fixes.md) - audit defects #14, #15, #16
- [remaining-gaps.md](remaining-gaps.md) - gap tracking
- [2026-03-30-runner-lane-architecture.md](2026-03-30-runner-lane-architecture.md) - lane/timeout architecture (now complete)
- [2026-02-25-orchestration-spine.md](2026-02-25-orchestration-spine.md) - daemon, FSMs, TaskStore, IPC, worktrees
- `src/agents/executor.rs` - agent task execution, action dispatch
- `src/daemon/handlers.rs` - RPC handlers including lock and work management
- `src/domain/lock.rs` - advisory lock domain model
- `src/domain/work.rs` - Work item domain model with dependencies
- `src/config.rs` - AgentRoleConfig with session_timeout_secs and max_iterations
- `src/tools/lane.rs` - lane policies and timeout defaults
- `src/tools/spawn.rs` - process group spawning with timeout/SIGTERM/SIGKILL
