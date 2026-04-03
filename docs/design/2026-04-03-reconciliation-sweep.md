# Design Document: Runtime Reconciliation Sweep

**Author:** Scott A. Idler
**Date:** 2026-04-03
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Add a two-layer reconciliation sweep that detects and recovers from state fractures where the DB (TaskStore) and physical state (git, worktrees, process handles) have diverged. The daemon handles process/DB-level reconciliation on a periodic timer. The Integrator handles git-level reconciliation as the first phase of each integration cycle. Every fix emits a `DaemonEvent::Reconciled` so the Coordinator can react.

## Problem Statement

### Background

The `encapsulate-fsm-status` refactor (v0.1.65) closed the "gatekeeper bypass" problem: the compiler now rejects direct `.status = X` mutations. Every status change goes through `transition()` (validated) or `force_status()` (greppable bypass). The FSM is no longer optional.

However, the FSM only governs **single-record mutations**. It cannot prevent **cross-system fractures** - cases where a multi-step operation succeeds partially:

1. Integrator runs `git merge --no-ff` (succeeds on disk)
2. Integrator calls `bundle.force_status(Merged)` (process crashes before this line)
3. Result: git says merged, DB says Integrating

The existing `recover_orphaned_records()` handles the simplest form of this at daemon startup: it resets any non-terminal record to a safe fallback state. But it has three gaps:

- **Startup only** - fractures that occur mid-session go undetected until the next restart
- **DB-only** - it reads TaskStore state but never checks git, worktrees, or process handles
- **Blind recovery** - it moves records to a fallback state without checking what actually happened on disk

### Problem

When the daemon crashes (or an agent panics) between a physical operation and its corresponding DB update, the system enters a fractured state. The current recovery is conservative (reset everything to safe fallback) but uninformed (doesn't check what actually happened). This leads to:

- Unnecessary work repetition: a successfully merged bundle gets reset to Accepted and re-merged
- Phantom state: a worktree exists for a Done work, wasting disk and confusing agents
- Stuck locks: an agent dies holding locks that never get released
- Invisible divergence: a force-push on the repo makes Published tick SHAs unreachable, but nothing detects this

### Goals

- Detect divergence between DB state and physical state (git, worktrees, process handles, locks)
- Automatically recover from recoverable fractures with correct state (not just fallback state)
- Emit structured events for every reconciliation action so the Coordinator can react
- Distinguish recoverable fractures from catastrophic ones (data loss / manual intervention required)
- Run both at startup and periodically during runtime

### Non-Goals

- This does NOT add new FSM transition rules or preconditions
- This does NOT change the `#[derive(Fsm)]` proc macro or the encapsulation from Option A
- This does NOT implement a distributed consensus protocol - Loopr is single-daemon
- This does NOT fix bugs in the merge logic itself - it detects and recovers from crashes during merge
- This does NOT add git hooks to the target repo (per CLAUDE.md: hooks belong in TARGET repos only, and Loopr orchestrates other repos)

## Proposed Solution

### Overview

Two reconciliation layers, each owned by the component that owns the ground truth:

1. **Daemon Sweep** (process/DB truth) - periodic `tokio::interval` task checking AgentSession vs process handles, Lock vs holder status, Work vs worktree existence
2. **Integrator Audit** (git truth) - first phase of each integration cycle checking branch existence, SHA reachability, merge ancestry

Both emit `DaemonEvent::Reconciled` events. The Coordinator reacts to these events the same way it reacts to any other state change.

### Architecture

```
Daemon Server
  |
  +-- accept_loop() ............. IPC + signal handling (existing)
  +-- run_supervisor() .......... Coordinator restart (existing)
  +-- run_worker() .............. Pull-based work assignment (existing)
  +-- run_reconciler() .......... NEW: periodic daemon sweep
  |     |
  |     +-- reconcile_sessions() .. AgentSession vs process handles
  |     +-- reconcile_locks() ..... Lock vs holder/work status
  |     +-- reconcile_worktrees() . Work vs filesystem
  |
  +-- Integrator agent
        |
        +-- run_cycle()
              |
              +-- audit_git_state() .. NEW: first step before recover_stuck_ticks
              |     |
              |     +-- audit_branches() .... Bundle vs branch existence
              |     +-- audit_tick_shas() ... Tick vs SHA reachability
              |     +-- audit_merge_ancestry() . Merged bundle vs main
              |
              +-- recover_stuck_ticks() .. (existing)
              +-- ... rest of cycle ...
```

### Event Design

A new `DaemonEvent` constructor for reconciliation actions:

```rust
impl DaemonEvent {
    /// Emitted when reconciliation detects and fixes a state fracture.
    pub fn reconciled(
        collection: &str,
        id: &str,
        from: &str,
        to: &str,
        reason: &str,
    ) -> Self {
        Self::new(
            "reconciliation.fixed",
            serde_json::json!({
                "collection": collection,
                "id": id,
                "from": from,
                "to": to,
                "reason": reason,
            }),
        )
    }

    /// Emitted when reconciliation detects a catastrophic fracture
    /// that requires manual intervention.
    pub fn reconciliation_failed(
        collection: &str,
        id: &str,
        status: &str,
        reason: &str,
    ) -> Self {
        Self::new(
            "reconciliation.failed",
            serde_json::json!({
                "collection": collection,
                "id": id,
                "status": status,
                "reason": reason,
                "severity": "catastrophic",
            }),
        )
    }
}
```

The `reason` field should use a fixed set of string constants (not free-form text) so the Coordinator can match on them programmatically:

```rust
// Recoverable reasons (used in reconciliation.fixed)
pub const REASON_MISSING_HANDLE: &str = "MissingHandle";
pub const REASON_HANDLE_FINISHED: &str = "HandleFinished";
pub const REASON_SESSION_TIMEOUT: &str = "SessionTimeout";
pub const REASON_HOLDER_TERMINAL: &str = "HolderTerminal";
pub const REASON_HOLDER_WORK_DONE: &str = "HolderWorkDone";
pub const REASON_LOCK_EXPIRED: &str = "LockExpired";
pub const REASON_STALE_WORKTREE: &str = "StaleWorktree";
pub const REASON_MISSING_BRANCH: &str = "MissingBranch";

// Catastrophic reasons (used in reconciliation.failed)
pub const REASON_SHA_UNREACHABLE: &str = "ShaUnreachable";
pub const REASON_SHA_MISSING: &str = "ShaMissing";
pub const REASON_MERGE_NOT_ANCESTOR: &str = "MergeNotAncestor";
```

The Coordinator consumes `reconciliation.fixed` events to decide follow-up actions (e.g., `MissingBranch` on a bundle means reassign the Work). The TUI displays `reconciliation.failed` events prominently - these require human attention.

### Observability

The primary monitoring channel is the daemon log (`~/.local/share/loopr/sessions/{session_id}/loopr.log`). An agent (or human) tailing this file can observe the reconciliation sweep without any TUI dependency.

**Log-level contract:**

| Level | What fires | Example |
|---|---|---|
| `info!` | Sweep heartbeat (every cycle) | `Reconciliation sweep completed: checked=42 fixed=0 catastrophic=0` |
| `warn!` | Recoverable fracture fixed | `Reconciliation: Lock lk-123 released (holder session terminal)` |
| `error!` | Catastrophic fracture detected | `Reconciliation CATASTROPHIC: Tick tk-456 integration_sha unreachable from main HEAD` |

The heartbeat log line is critical. Silence in logs is ambiguous - it could mean "healthy" or "sweep isn't running." The heartbeat confirms liveness and provides a per-cycle summary. A monitoring agent can detect:

- **Healthy:** heartbeat fires every 60s with `fixed=0`
- **Recovering:** heartbeat fires with `fixed>0` (fractures found and auto-fixed)
- **Degraded:** `error!` line fires with "CATASTROPHIC" prefix
- **Sweep dead:** no heartbeat for >2 intervals (monitoring agent should alert)

**TaskStore polling:** A monitoring agent can also read JSONL/SQLite records directly and check invariants:

- No Work has been InProgress for >N minutes without a non-terminal AgentSession
- No Bundle has been Integrating for >N minutes
- All Published Ticks have `integration_sha` set
- No Active Locks have terminal holder sessions

This gives two independent signal paths (log tailing + state polling) that an agent can use to determine system health without depending on the TUI.

**`system.status` extension:** The `system.status` IPC response should include reconciliation health:

```json
{
  "reconciliation": {
    "last_sweep_at": 1743648123456,
    "checked": 42,
    "fixed": 0,
    "catastrophic": 0,
    "degraded": false
  }
}
```

### Reconciliation Rules

#### Daemon Sweep: AgentSession vs Process Handles

| DB State | Physical State | Severity | Recovery |
|---|---|---|---|
| Running/WaitingForLlm | No task handle in `agent_handles` | Recoverable | `force_status(Failed)`, emit Reconciled |
| Running/WaitingForLlm | Task handle exists but `.is_finished()` | Recoverable | Read task result, set Completed or Failed accordingly, emit Reconciled |
| Starting | No task handle, age > `session_timeout_secs` | Recoverable | `force_status(Failed)`, emit Reconciled |
| Completed/Failed | Task handle still in map | Cleanup | Remove handle from map, no status change |

#### Daemon Sweep: Lock vs Holder Status

Lock's `holder_id` is a Work ID. To check holder session status: find any non-terminal AgentSession with `work_id == lock.holder_id`. If none exist, the holder agent is dead.

| DB State | Physical State | Severity | Recovery |
|---|---|---|---|
| Active | No non-terminal session with `work_id == holder_id` | Recoverable | `lock.release()`, emit Reconciled |
| Active | Holder work is Done/Abandoned | Recoverable | `lock.release()`, emit Reconciled |
| Active | `expires_at` in the past | Recoverable | `lock.expire()`, emit Reconciled (already exists in Gap #30) |

#### Daemon Sweep: Work vs Worktree

The reconciler needs access to `WorktreeManager` (currently passed to handlers). The `run_reconciler()` task receives a clone of `WorktreeManager` at spawn time, same pattern as `run_worker()`.

| DB State | Physical State | Severity | Recovery |
|---|---|---|---|
| Done/Abandoned | Worktree directory exists AND no non-terminal agent session for this work | Cleanup | `worktree.cleanup()`, emit Reconciled |
| InProgress | No worktree, no active agent | Recoverable | `force_status(Blocked)`, emit Reconciled (already exists) |

#### Integrator Audit: Bundle vs Branch

| DB State | Physical State | Severity | Recovery |
|---|---|---|---|
| Proposed/Triaged/Reviewed/Accepted | Branch `agent/{work_id}` missing | Recoverable | `force_status(Rejected)`, emit Reconciled |
| Integrating | Branch missing | Recoverable | `force_status(Rejected)`, emit Reconciled |
| Merged | Skip branch-existence check | N/A | Branch may have been deleted after tick publish - this is normal lifecycle |
| Merged | Bundle's `head_commit` not reachable from its Tick's `integration_sha` | Catastrophic | Emit ReconciliationFailed, do NOT auto-fix |
| Any non-terminal | `head_commit` doesn't match branch HEAD | Info | Log warning only - implementer may have added commits post-proposal |

#### Integrator Audit: Tick SHA Reachability

| DB State | Physical State | Severity | Recovery |
|---|---|---|---|
| Published | `integration_sha` not reachable from main HEAD | Catastrophic | Emit ReconciliationFailed, enter degraded mode |
| Published | `integration_sha` is None | Bug | Log error, emit ReconciliationFailed |
| Published | `integration_sha` reachable but not HEAD | Normal | No action - subsequent ticks advance HEAD |

#### Degraded Mode

When a catastrophic fracture is detected, the system enters degraded mode:

- The Integrator stops creating new Ticks
- The Coordinator stops spawning new Implementers (no new git-writing work)
- Researchers, Reviewers, and existing agents continue (read-only work, finish current iterations)
- The TUI shows a persistent warning banner
- A `DaemonEvent::reconciliation_failed` is emitted for logging/alerting
- Recovery requires human intervention (inspect git state, potentially `git reflog` to recover)

Degraded mode is a flag on `Stores` (e.g., `stores.degraded: AtomicBool`). The Integrator checks this flag at the start of each cycle and skips tick creation if set. The flag is cleared by an explicit daemon IPC command (`system.clear_degraded`).

The flag is not persisted to disk. On daemon restart, the Integrator's first `audit_git_state()` call will re-detect the catastrophic condition and re-set the flag. This is correct because the Integrator is the authority on git state - the daemon startup sweep should not attempt SHA reachability checks.

### Data Model

No new domain structs. The reconciliation state is ephemeral - it runs, emits events, and the results are captured in the existing domain records via `force_status()` and the event log.

The only new runtime state is the `degraded` flag on Stores (`AtomicBool`, not persisted - see Degraded Mode section for restart behavior).

### Implementation Plan

#### Phase 1: Event Contract and Observability

Define the `DaemonEvent::reconciled` and `DaemonEvent::reconciliation_failed` constructors. Add the sweep heartbeat log line (`info!` at end of each `reconcile()` call with checked/fixed/catastrophic counts). Extend the `system.status` IPC response with reconciliation health fields. Wire the Coordinator to consume `reconciliation.fixed` events (at minimum: log them; stretch: auto-react with reassignment).

Set up the dedicated reconciliation log file (`sessions/{session_id}/reconciliation.log`). Define the typed reason constants.

**Files:** `src/ipc/protocol.rs`, `src/daemon/handlers/system.rs`, `src/daemon/context.rs`

#### Phase 2: Extract and Expand Daemon Sweep

Refactor `recover_orphaned_records()` into a reusable `DaemonContext::reconcile()` method. The existing startup call becomes `self.reconcile()`. Then add the missing checks:

- Lock holder-status-aware release (no non-terminal session with matching `work_id`)
- Stale worktree cleanup (worktree exists but Work is Done/Abandoned)
- Session vs handle cross-check (handle `.is_finished()` but session not terminal)

Emit `DaemonEvent::reconciled` for each fix (requires `event_tx` to be passed to `reconcile()`).

**Files:** `src/daemon/context.rs`, `src/daemon/mod.rs`

#### Phase 3: Make Daemon Sweep Periodic

Spawn a `run_reconciler()` background task in `daemon_main()` with a configurable interval (default: 60s). The task calls `ctx.reconcile()` on each tick and checks `stores.shutting_down` to exit gracefully.

Add `ReconcilerConfig` to the daemon config:
```rust
pub struct ReconcilerConfig {
    pub interval_secs: u64,    // default: 60
    pub enabled: bool,         // default: true
}
```

**Files:** `src/daemon/mod.rs`, `src/config.rs`

#### Phase 4: Integrator Git Audit

Add `audit_git_state()` as the first step of the Integrator's `run_cycle()`, before `recover_stuck_ticks()`. This method:

1. `audit_branches()` - for each non-terminal Bundle, verify `agent/{work_id}` branch exists via `git rev-parse`
2. `audit_tick_shas()` - for each Published Tick with `integration_sha`, verify reachability via `git merge-base --is-ancestor`
3. `audit_merge_ancestry()` - for each Merged Bundle, verify `head_commit` is reachable from its Tick's `integration_sha` via `git merge-base --is-ancestor`

Add `degraded` flag to Stores. Add `system.clear_degraded` IPC command.

**Files:** `src/agents/integrator.rs`, `src/daemon/stores.rs`, `src/daemon/handlers/system.rs`

## Alternatives Considered

### Alternative 1: Dedicated Reconciliation Agent (Lifeguard)

- **Description:** A new agent role that runs periodically, reads all stores + git state, emits findings as structured actions.
- **Pros:** Clean separation of concerns. Could use LLM for judgment calls.
- **Cons:** Adds a new agent type with its own session lifecycle, supervisor logic, and coordination overhead. Most reconciliation is deterministic and doesn't need an LLM.
- **Why not chosen:** The reconciliation logic is deterministic rule application, not intelligence. Adding agent overhead for deterministic work is overengineering.

### Alternative 2: All Reconciliation in the Integrator

- **Description:** Move all reconciliation (including session/lock/worktree checks) into the Integrator.
- **Pros:** Single reconciliation owner.
- **Cons:** The Integrator doesn't own process handles or lock state. It would need to reach across domain boundaries. If the Integrator is down, no reconciliation runs.
- **Why not chosen:** Violates domain ownership. The daemon owns process state; the Integrator owns git state.

### Alternative 3: Reconciliation as a One-Shot CLI Command

- **Description:** A `loopr reconcile` CLI command that scans and fixes state on demand.
- **Pros:** Zero runtime overhead. Human-triggered.
- **Cons:** Doesn't catch fractures until someone manually runs it. Defeats the purpose of autonomous recovery.
- **Why not chosen:** The whole point is detecting fractures without human intervention. A CLI command is a useful addition but not a substitute for runtime detection.

## Technical Considerations

### Dependencies

No new crates. Git operations use the same `std::process::Command` wrappers the Integrator already uses. The periodic task uses `tokio::time::interval`.

### Performance

- **Daemon sweep:** Reads in-memory HashMaps (fast). Lock holder cross-check is O(sessions * locks) but both sets are small.
- **Integrator audit:** Git operations (`rev-parse`, `merge-base --is-ancestor`) are fast for small repos. For large repos with many published ticks, limit the SHA reachability check to the last N ticks (configurable, default: 10).
- **Interval:** 60s for daemon sweep. Integrator audit runs every cycle (15s default). Both are lightweight relative to actual agent work.

### Testing Strategy

- Unit tests for each reconciliation rule (mock stores with specific fracture states, verify correct recovery)
- Integration tests that simulate crash scenarios (create a fractured state, run sweep, verify recovery + event emission)
- The existing `recover_orphaned_records` tests serve as the baseline - extend them with the new checks

### Rollout Plan

One commit per phase. Each phase compiles and passes `otto ci` independently. Phase 1 (events) and Phase 2 (startup expansion) are safe to ship immediately. Phase 3 (periodic) and Phase 4 (git audit) can be feature-flagged via config if needed.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Sweep fixes state while an operation is in progress | Medium | High | Daemon sweep only touches records that have been in a non-terminal state for longer than a threshold (e.g., 30s for sessions, 5min for ticks). Integrator audit runs at cycle start, before any mutations. |
| Git operations in audit phase slow down integration cycle | Low | Medium | Limit SHA reachability checks to last N ticks. `git rev-parse` and `merge-base --is-ancestor` are O(1) for reachable refs. |
| False-positive catastrophic detection after legitimate force-push | Low | High | Catastrophic events don't auto-fix - they require human intervention. The human can inspect and clear degraded mode. |
| Sweep emits too many events, flooding Coordinator | Low | Low | Events are only emitted when state actually changes. A stable system produces zero reconciliation events. |
| Race between sweep and normal agent operations | Medium | Medium | Use the existing RwLock guards on stores. The sweep holds write locks briefly (same pattern as existing recovery). |
| Daemon sweep and Integrator audit fix the same record simultaneously | Low | Medium | The Integrator audit only touches Bundle and Tick records. The daemon sweep only touches Session, Lock, and Work records. No overlap by design - domain ownership prevents races. |

## Resolved Questions

- [x] **Sweep interval: fixed (60s).** Adaptive intervals add complexity to a safety net. If the system is unstable, increasing sweep frequency could create a death spiral by consuming more resources during a crisis. Predictability is the primary requirement for a recovery system.
- [x] **Degraded mode scope: block Tick creation AND new Implementer assignments.** Researchers and Reviewers continue (read-only, don't touch git refs). Existing agents finish their current iteration to avoid losing mid-flight LLM work. The Coordinator checks the `degraded` flag before spawning Implementers. The Integrator checks it before creating Ticks.
- [x] **Audit scope for Merged bundles: last 100 bundles or 30 days (whichever is smaller).** Checking every bundle in a year-old repo every 15s integration cycle will degrade performance. Fractures almost always occur near the tip. The Integrator tracks the cutoff and skips older records.
- [x] **Dedicated reconciliation log: accepted.** Write to `~/.local/share/loopr/sessions/{session_id}/reconciliation.log` in addition to the daemon log. High-signal, low-volume file - empty in a healthy system, the "black box" during failures. Format: `[timestamp LEVEL] collection:id from->to reason`.
- [x] **Capture `from` state before `force_status()`.** Every `reconcile()` fix must read the current status before calling `force_status()`, so the `DaemonEvent::reconciled` event carries the actual `from` value. Needed for the Coordinator to make informed follow-up decisions.
- [x] **Typed reason constants.** Fixed string constants (not free-form text) so the Coordinator can match programmatically. See Event Design section.

## Open Questions

None. All resolved.

## References

- `docs/design/2026-04-02-encapsulate-fsm-status.md` - Option A (the gatekeeper) that this builds on
- `docs/design/2026-04-01-derive-fsm.md` - the `#[derive(Fsm)]` design
- `src/daemon/context.rs:411-530` - existing `recover_orphaned_records()` implementation
- `src/agents/integrator.rs:135-231` - existing `recover_stuck_ticks()` in the Integrator
- `src/agents/integrator.rs:885-952` - `merge_bundle_branches()` git operations
- `src/ipc/protocol.rs:151-334` - existing DaemonEvent constructors (pattern for new events)
