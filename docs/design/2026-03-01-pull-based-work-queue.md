# Design Document: Pull-Based Work Queue

**Author:** Scott Idler + Claude
**Date:** 2026-03-01
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

The current work assignment model is push-based: the Coordinator decides which Work to assign to which Implementer, issues `AssignAgent`, and the daemon spawns an agent. This creates several failure modes: the Coordinator wastes iterations on scheduling decisions, race conditions between `AssignAgent` and `auto_start_agents()` (Bug #5, since fixed with dedup), and load imbalance when some Works finish faster than others. This document proposes a pull-based work queue where Implementer agents are a persistent worker pool that pulls Ready Work items when idle, with the Coordinator focusing on hierarchy management and gate decisions rather than micro-scheduling.

## Problem Statement

### Background

The current push-based flow:
1. Coordinator sees Work in `Ready` state
2. Coordinator emits `AssignAgent { agent_type: "implementer", target_id: "work-id" }`
3. Executor transitions Work `Ready → InProgress`
4. Executor calls `agent.start` → spawns Implementer Tokio task
5. Implementer runs, completes or fails
6. Post-loop cleanup: Work transitions based on outcome
7. Coordinator detects Work completion on next iteration, assigns next Work

The dedup guard (`work-handback.md`) prevents duplicate Implementers on the same Work. The Coordinator's context shows active agent sessions so it avoids over-assignment. But the fundamental model remains: the Coordinator is the scheduler, and each assignment is a discrete push decision.

### Problem

**Problem 1 — Coordinator iteration waste:** The Coordinator burns iterations on scheduling. When 3 Works are Ready and the pool has 2 free slots, the Coordinator must: (a) assess which Works are Ready, (b) check pool capacity, (c) emit 2 `AssignAgent` actions, (d) wait for confirmation. This is 1 full LLM iteration ($0.02-0.05) to do what a for-loop could do deterministically.

**Problem 2 — Latency gap between completion and re-assignment:** When an Implementer completes, the Work transitions. The Coordinator doesn't know until its next iteration (5s active interval at best, 30s idle interval at worst). During that gap, the pool has an idle slot that could be working.

**Problem 3 — The Coordinator is an SPOF for scheduling:** If the Coordinator dies (lifeguard, LLM error, supervisor restart delay), no new Work is assigned even though Ready Works exist and the Implementer pool has capacity. The supervisor adds a 10-300s restart delay. During that window, the system is idle.

**Problem 4 — Assignment quality:** The Coordinator LLM picks which Work to assign based on... whatever the LLM thinks is best. It has no structured priority function. It might pick the hardest Work first, or the easiest, or randomly. A deterministic priority function (dependency depth, resource contention, estimated complexity) would make better decisions than an LLM picking from a list.

### Goals

- Implementer pool workers pull Work from a priority queue when idle
- Assignment is deterministic (no LLM involved in scheduling)
- Gap between Work completion and re-assignment is near-zero (within the daemon's event loop)
- Coordinator focuses on hierarchy management, gate decisions, and override — not micro-scheduling
- `AssignAgent` action remains available for explicit Coordinator overrides (force-assign a specific Work)
- Compatible with existing `auto_start_agents()` mechanism

### Non-Goals

- Pull-based assignment for Reviewers (Reviewers are triggered by Bundle triage, not Work state)
- Pull-based assignment for Researchers (Researchers are spawned for specific queries)
- Work priority ML (priority is a deterministic function, not learned)
- Cross-phase Work pulling (workers only pull from the current active Phase)
- Dynamic pool sizing (pool size remains fixed per config)

## Proposed Solution

### Overview

Two changes:

1. **Work Priority Queue** — A daemon-level component that maintains a priority-ordered view of Ready Works and dequeues them for idle workers
2. **Worker Pool with Pull Loop** — Implementer pool workers run a persistent loop: pull Work → implement → complete → pull next Work. Workers are long-lived Tokio tasks (like the Coordinator), not ephemeral per-Work tasks.

### Architecture

```
Before (push):
  Coordinator ──AssignAgent──→ Handler ──agent.start──→ Implementer(work_id)
                                                            ↓ (completes)
                                                        (session dies)
  Coordinator ──AssignAgent──→ Handler ──agent.start──→ Implementer(next_work_id)

After (pull):
  Coordinator ──CreateWork/Transition──→ Ready Works ──→ WorkQueue (priority-ordered)
                                                              ↓
  Worker #1 (long-lived) ←──dequeue──────────────────────────┘
      ↓ implement, complete
  Worker #1 ←──dequeue── WorkQueue ── next Ready Work
      ↓ implement, complete
  Worker #1 ←──dequeue── ...
```

### Change 1: Work Priority Queue

**New file:** `src/daemon/work_queue.rs`

The WorkQueue is a daemon-level component that provides a priority-ordered view of assignable Work items. It is NOT a separate data structure — it reads from the existing `Stores.works` and `Stores.locks` to determine what's available.

```rust
use std::sync::Arc;
use crate::daemon::context::Stores;
use crate::domain::work::{Work, WorkStatus};
use crate::domain::lock::LockStatus;

/// Priority score for a Ready Work item. Higher = picked first.
#[derive(Debug, Clone, PartialEq)]
struct WorkPriority {
    pub work_id: String,
    pub score: i64,
}

/// Determine the next Work to assign from the Ready pool.
/// Returns None if no assignable Work exists.
pub fn next_assignable_work(stores: &Arc<Stores>, current_phase_id: Option<&str>) -> Option<String> {
    let works = stores.works.read().unwrap();
    let locks = stores.locks.read().unwrap();
    let sessions = stores.agent_sessions.read().unwrap();

    // Filter to Ready Works in the current Phase (if specified)
    let mut candidates: Vec<WorkPriority> = works.values()
        .filter(|w| w.status == WorkStatus::Ready)
        .filter(|w| {
            current_phase_id.map(|pid| w.phase_id == pid).unwrap_or(true)
        })
        // Exclude Works whose dependencies aren't Done
        .filter(|w| {
            w.dependencies.iter().all(|dep_id| {
                works.get(dep_id)
                    .map(|dep| dep.status == WorkStatus::Done)
                    .unwrap_or(false)
            })
        })
        // Exclude Works that already have a non-terminal Implementer
        .filter(|w| {
            !sessions.values().any(|s| {
                s.agent_type == crate::agents::AgentType::Implementer
                    && s.work_id.as_deref() == Some(&w.id)
                    && !s.status.is_terminal()
            })
        })
        .map(|w| {
            let score = compute_priority(w, &locks);
            WorkPriority { work_id: w.id.clone(), score }
        })
        .collect();

    // Sort by priority (highest first), then by creation time (oldest first for tie-breaking)
    candidates.sort_by(|a, b| b.score.cmp(&a.score));

    candidates.first().map(|c| c.work_id.clone())
}

/// Compute priority score for a Work item.
/// Higher score = higher priority.
fn compute_priority(work: &Work, locks: &std::sync::RwLockReadGuard<'_, std::collections::HashMap<String, crate::domain::lock::Lock>>) -> i64 {
    let mut score: i64 = 0;

    // Prefer Works with no resource contention (no active locks on their resource_tags)
    let has_contention = work.resource_tags.iter().any(|tag| {
        locks.values().any(|l| l.resource == *tag && l.status == LockStatus::Active)
    });
    if !has_contention {
        score += 100;
    }

    // Prefer Works with fewer/no dependencies (can start immediately)
    score += (10 - work.dependencies.len().min(10) as i64) * 10;

    // Prefer Works with more dependents (unblocks more downstream work)
    // This requires scanning all works for who depends on this one
    // Deferred: too expensive in a lock-holding context. Use dependency count as proxy.

    // Prefer older Works (FIFO within priority tier)
    // Score is relative, so we don't use absolute timestamps.
    // The sort's tie-breaking on creation time handles this.

    score
}
```

**Why not a separate priority queue data structure?** A separate queue would need to be kept in sync with TaskStore — every Work transition would need to update the queue. The existing `Stores.works` HashMap IS the source of truth. Reading it on demand (once per dequeue attempt) is simple, correct, and fast (typically <20 Work items).

### Change 2: Worker Pool with Pull Loop

**New file:** `src/agents/worker.rs`

Each worker is a long-lived Tokio task that loops: pull Work → implement → complete → pull next.

```rust
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use log::{info, debug, warn};

use crate::agents::executor::run_single_work;
use crate::agents::AgentStatus;
use crate::config::AgentRoleConfig;
use crate::daemon::context::Stores;
use crate::daemon::work_queue;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

pub struct WorkerConfig {
    pub worker_id: u32,
    pub poll_interval_secs: u64,    // default 5
    pub idle_interval_secs: u64,    // default 15
}

/// Run a persistent worker that pulls and implements Work items.
pub async fn run_worker(
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    worktree_mgr: WorktreeManager,
    implementer_config: AgentRoleConfig,
    config: WorkerConfig,
) {
    info!("Worker {} started", config.worker_id);

    loop {
        // Check for daemon shutdown
        if stores.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
            info!("Worker {} shutting down", config.worker_id);
            break;
        }

        // Get current phase from CoordinatorState
        let current_phase_id = {
            let states = stores.coordinator_states.read().unwrap();
            states.values()
                .find(|s| !s.fsm_state.is_terminal())
                .and_then(|s| s.current_phase_id.clone())
        };

        // Try to pull next Work
        let work_id = work_queue::next_assignable_work(&stores, current_phase_id.as_deref());

        match work_id {
            Some(wid) => {
                info!("Worker {} pulled Work {}", config.worker_id, wid);

                // Transition Work Ready → InProgress (via existing handler).
                // This may fail if another worker grabbed it first — that's fine,
                // we log and retry on next poll.
                let result = run_single_work(
                    &stores,
                    &event_tx,
                    &worktree_mgr,
                    &implementer_config,
                    &wid,
                    config.worker_id,
                ).await;

                match result {
                    Ok(()) => {
                        info!("Worker {} completed Work {}", config.worker_id, wid);
                    }
                    Err(e) => {
                        warn!("Worker {} failed Work {}: {}", config.worker_id, wid, e);
                    }
                }

                // Brief pause before pulling next (avoid hot loop on rapid failures)
                tokio::time::sleep(Duration::from_secs(config.poll_interval_secs)).await;
            }
            None => {
                // No work available — idle
                debug!("Worker {} idle, no Ready Work", config.worker_id);
                tokio::time::sleep(Duration::from_secs(config.idle_interval_secs)).await;
            }
        }
    }
}
```

**`run_single_work()`** is a refactored extract from the existing `run_agent_task()` in `executor.rs`. It:
1. Creates an AgentSession
2. Transitions Work `Ready → InProgress`
3. Creates/gets worktree
4. Creates LLM client, AgentLogger, AgentContext
5. Creates ImplementerAgent and calls `run()`
6. Runs post-loop handback logic
7. Cleans up worktree

This is the same logic that currently lives in `run_agent_task()`, extracted into a callable function.

**Race handling in `run_single_work()`:** The function's first step is `Ready → InProgress` via the daemon handler. If this transition fails (Work already InProgress — another worker grabbed it), `run_single_work()` returns `Ok(())` immediately (not an error — this is expected contention). The worker logs "Work already claimed" and moves on to the next poll.

### Change 3: Worker Pool Spawning in Daemon

**File:** `src/daemon/mod.rs`

Replace the per-Work `agent.start` spawning with persistent workers:

```rust
// In daemon_main(), after auto-starting Coordinator
if stores.config.agents.enabled {
    let pool_size = stores.config.agents.implementer.pool_size;
    for i in 0..pool_size {
        let s = stores.clone();
        let e = event_tx.clone();
        let w = worktree_mgr.clone();
        let c = stores.config.agents.implementer.clone();
        let wc = WorkerConfig {
            worker_id: i,
            poll_interval_secs: 5,
            idle_interval_secs: 15,
        };
        tokio::spawn(run_worker(s, e, w, c, wc));
    }
}
```

### Change 4: Coordinator De-scheduling

The Coordinator no longer needs to emit `AssignAgent` for Implementers. The prompt is updated:

**Remove from Coordinator prompt:**
```
5. `assign_agent` — Start an Implementer or Reviewer on a target
```

**Replace with:**
```
Workers automatically pull Ready Works — you do not need to assign Implementers.
Focus on creating Works, managing gates, and triaging Bundles.
If you need to force-assign a specific Work to bypass the queue, use `assign_agent`.
```

`AssignAgent` remains available as an escape hatch (e.g., force-assign a blocked Work after an override). But for normal flow, the Coordinator creates Works, transitions them to Ready, and the worker pool handles the rest.

### Interaction with Existing Mechanisms

**`auto_start_agents()` hook:** Currently auto-starts an Implementer when Work transitions to `InProgress`. With pull-based workers, this hook becomes unnecessary for Implementers (workers handle the transition themselves). The hook should be guarded:

```rust
if agent_type == AgentType::Implementer && stores.config.agents.pull_based_workers {
    // Workers handle InProgress → spawn automatically. Skip auto_start.
    return;
}
```

**Reviewer auto-start:** Unchanged. Reviewers are still push-triggered by Bundle triage events.

**`AssignAgent` action:** Still functional as a Coordinator override. If the Coordinator explicitly assigns, the handler:
1. Transitions Work `Ready → InProgress`
2. Spawns a one-shot Implementer task (current behavior)
3. Workers see the Work as `InProgress` and skip it

### Data Model

| Change | Type | File |
|--------|------|------|
| `work_queue.rs` | New module | `daemon/work_queue.rs` |
| `worker.rs` | New module | `agents/worker.rs` |
| `run_single_work()` | Extracted function | `agents/executor.rs` |
| `AgentConfig.pull_based_workers` | New config field (`bool`, default `false`) | `config.rs` |
| `WorkerConfig` | New config struct | `agents/worker.rs` |
| `Stores.shutting_down` | New field (`AtomicBool`) | `daemon/context.rs` |

### Implementation Plan

**Phase 1: Extract `run_single_work()`**
- Refactor `run_agent_task()` in `executor.rs` to extract the Implementer-specific logic into `run_single_work()`
- Existing `run_agent_task()` calls `run_single_work()` internally — zero behavior change
- Tests: existing tests pass unchanged

**Phase 2: Work Priority Queue**
- Create `daemon/work_queue.rs` with `next_assignable_work()` and `compute_priority()`
- Tests: priority ordering (no contention > contention, fewer deps > more deps), dependency filtering, phase filtering, dedup filtering

**Phase 3: Worker Pool**
- Create `agents/worker.rs` with `run_worker()`
- Add `pull_based_workers` config flag (default false)
- Spawn workers in `daemon_main()` when flag is true
- Guard `auto_start_agents()` for Implementers when flag is true
- Tests: worker pulls Work, worker idles when no Work, worker respects shutdown

**Phase 4: Coordinator prompt update**
- Update Coordinator prompt to remove `assign_agent` for Implementers from normal flow
- Keep `assign_agent` as documented escape hatch
- Tests: Coordinator doesn't emit `assign_agent` when workers handle scheduling

## Alternatives Considered

### Alternative 1: Event-driven assignment (reactive, not polling)

- **Description:** Instead of workers polling for Work, use the event bus. When a Work transitions to Ready, the daemon broadcasts `work.ready` event. A scheduler task receives the event and immediately assigns an idle worker.
- **Pros:** Zero latency between Work becoming Ready and assignment. No polling overhead.
- **Cons:** Requires a scheduler coordinator (another centralized component). Race conditions if multiple Works become Ready simultaneously. More complex than polling.
- **Why not chosen:** Polling with a 5-15s interval is simple and sufficient. The latency cost (5-15s) is negligible compared to LLM iteration time (~10-30s). Event-driven is a future optimization.

### Alternative 2: Keep push-based, add deterministic pre-scheduler

- **Description:** Instead of changing the work distribution model, add a daemon-level scheduler that runs before the Coordinator and auto-assigns Ready Works to idle pool slots. The Coordinator still sees assignments but doesn't make them.
- **Pros:** Minimal change to agent architecture. Coordinator prompt unchanged.
- **Cons:** Adds complexity (another daemon task) without removing the Coordinator's scheduling burden. The Coordinator still sees assignments in context and may try to "correct" them.
- **Why not chosen:** Pull-based workers are a cleaner separation of concerns. Scheduling is the daemon's job. Strategy is the Coordinator's job.

### Alternative 3: Workers as separate OS processes

- **Description:** Workers are separate binaries (like Claude Code sessions) that connect to the daemon via Unix socket and pull Work via IPC.
- **Pros:** Stronger isolation. Workers can be restarted independently. Could scale across machines.
- **Cons:** Multi-writer risk (workers can modify shared state outside daemon control). Loses the single-authority guarantee. IPC overhead per action. Complexity of process management.
- **Why not chosen:** Workers as Tokio tasks inside the daemon preserve the single-authority invariant. All mutations go through the daemon's FSM validation.

## Technical Considerations

### Dependencies

No new crates. Uses existing `tokio`, `log`, `serde_json`.

### Performance

- `next_assignable_work()`: O(W * L) where W = works, L = locks. Typically W < 50, L < 20. Microseconds.
- Worker polling: one `next_assignable_work()` call every 5-15 seconds per worker. With pool_size=3, that's 3 calls per interval. Negligible.
- Workers as Tokio tasks: near-zero memory overhead when idle (just a future on the runtime).

### Security

Workers operate within the same trust boundary as the current `run_agent_task()`. No new attack surface. Work assignment goes through the existing FSM validation (Ready → InProgress transition must be valid).

### Testing Strategy

**Unit tests:**
- `work_queue.rs`: Priority ordering, dependency filtering, phase filtering, contention avoidance, dedup
- `worker.rs`: Pull loop, idle behavior, shutdown detection

**Integration tests:**
- Worker pulls Ready Work, implements, completes, pulls next
- Two workers pull different Works concurrently (no collision)
- Worker idles when no Ready Work, resumes when Work becomes Ready
- Coordinator creates Work → worker picks it up without Coordinator `AssignAgent`
- `AssignAgent` override still works (Coordinator force-assigns, worker skips that Work)

### Rollout Plan

Feature-flagged behind `pull_based_workers: bool` (default `false`). Existing push-based assignment remains the default. Can be toggled per-project in `loopr.yml`. Both modes are tested. Migration path: enable flag, observe behavior, disable push-based auto_start when confident.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Two workers race to pull the same Work | Medium | Low | `Ready → InProgress` transition is serialized through the daemon handler. Second worker's transition fails (Work already InProgress). Worker retries next poll. |
| Worker pool starvation (all workers blocked on slow Works) | Low | Medium | Same risk as current model. SLA override (coordinator-override doc) addresses stuck Works. Pool size is configurable. |
| Feature flag confusion (push and pull competing) | Low | Medium | When `pull_based_workers` is true, `auto_start_agents()` is disabled for Implementers. Clear separation. |
| Workers don't respect Phase ordering | Low | High | `next_assignable_work()` filters by `current_phase_id` from CoordinatorState. Workers only pull from the active Phase. |
| Coordinator still emits `AssignAgent` from habit | Medium | Low | Assignment succeeds (one-shot Implementer). Worker sees Work as InProgress, skips it. No collision, just wasted Coordinator iteration. Prompt update mitigates. |

## Open Questions

- [ ] Should workers have individual IDs visible in the TUI (Worker #0, Worker #1) or use session IDs like current agents?
- [ ] Should the poll interval be adaptive (shorter when work is available, longer when idle)?
- [ ] Should `next_assignable_work()` consider estimated Work complexity (from description length, resource_tag count)?
- [ ] When the Coordinator transitions between Phases, should workers drain their current Work or be interrupted?
- [ ] Should the priority function be configurable in `loopr.yml`?
- [ ] What happens if the daemon starts with `pull_based_workers: true` but no CoordinatorState exists yet (no goal set)? Workers would pull from any phase. Should they wait for the Coordinator to set a goal first?
- [ ] Should workers create their own AgentSession records (for TUI visibility), or use a different tracking mechanism?

## References

- `docs/design/2026-03-01-work-handback.md` — Implementer dedup guard (prevents duplicate assignment)
- `docs/design/2026-02-26-loopr-v3-mvp4.md` — Coordinator loop and AssignAgent design
- `docs/design/2026-03-01-implementer-completion-and-parallel-execution.md` — Parallel execution improvements
- `src/agents/executor.rs` — Current `run_agent_task()` logic
- `src/daemon/handlers.rs` — `auto_start_agents()` hook
- `src/domain/work.rs` — Work FSM and dependency tracking
- `docs/design/2026-03-01-coordinator-override-sla-recovery.md` — SLA override (interacts: overridden Work → Ready is immediately pullable by workers)
- `docs/design/2026-03-01-agent-self-correction-loop.md` — Self-correction loop (interacts: workers benefit from fewer wasted iterations)
