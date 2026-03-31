# Design Document: File-Touch Broadcasting via Advisory Locks

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Superseded by [2026-03-30-pipeline-hardening-locks-timeouts.md](2026-03-30-pipeline-hardening-locks-timeouts.md)
**Review Passes Completed:** 5/5

## Summary

Implementers currently write files without signaling which paths they're touching. Neither the Coordinator nor sibling agents can detect resource contention until merge time. This document proposes auto-acquiring advisory locks on every `WriteFile` action, giving real-time visibility into which agent is modifying which files. Under `LockStrict`, this also provides hard blocking of conflicting writes.

## Problem Statement

### Background

The advisory lock system is fully implemented:
- `Lock` records with `resource`, `holder_id`, `status` (Active/Released/Expired)
- `lock.create`, `lock.list`, `lock.release` handlers
- `AcquireLock`/`ReleaseLock` agent actions
- `LockStrict` policy that blocks `WriteFile` to locked paths
- `build_state_summary()` in the Coordinator already displays active locks

But implementers don't use any of it. Files are written silently.

### Problem

**No visibility into file contention:**

1. Implementer A (Work: "Add auth module") writes `src/main.py`
2. Implementer B (Work: "Add logging") also writes `src/main.py`
3. Neither agent knows the other is touching the same file
4. The Coordinator's state summary shows no lock activity
5. Conflict is invisible until the Integrator tries to merge both Bundles

Under `LockAdvisory` (default), this isn't a hard failure — conflicts get resolved at merge — but it wastes work. An Implementer might spend 15 iterations building code that will conflict with a sibling's changes.

Under `LockStrict`, the protection exists but is useless because nobody acquires locks in the first place.

### Goals

- Make file touches visible in real-time via advisory locks
- Enable the Coordinator to detect contention in its state summary
- Enable `LockStrict` to actually work (requires locks to exist)
- Zero changes to agent prompts or LLM behavior
- Clean up locks automatically when agents exit

### Non-Goals

- Automatic conflict resolution or file merging
- Mandatory locking (LockAdvisory remains the default)
- Cross-repo or cross-worktree file tracking
- Tracking file reads (only writes create contention)

## Proposed Solution

### Overview

Two changes:

1. **Auto-lock on `WriteFile`** — every write acquires an advisory lock on the path, keyed to the agent's `work_id`
2. **Lock cleanup on agent exit** — release all locks held by the agent's `work_id` when it terminates

### Change 1: Auto-lock on WriteFile

In `execute_action()`, inside the `WriteFile` handler, after the sandbox check and before the existing `LockStrict` check:

```rust
AgentAction::WriteFile { path, content } => {
    let full_path = sandbox::validate_sandboxed_path(worktree_path, path, false)?;

    // Auto-acquire advisory lock on the file for this work_id.
    // Under LockAdvisory: purely informational — visible in state summary.
    // Under LockStrict: the existing check below blocks conflicting writes.
    if let Some(wi_id) = work_id {
        let check_resp = bridge.request(
            "lock.list",
            serde_json::json!({ "resource": path, "active_only": true }),
        );
        let already_held = check_resp
            .result
            .as_ref()
            .and_then(|v| v.as_array())
            .map(|locks| locks.iter().any(|l| {
                l.get("holder_id").and_then(|v| v.as_str()) == Some(wi_id)
            }))
            .unwrap_or(false);

        if !already_held {
            let _ = bridge.request(
                "lock.create",
                serde_json::json!({
                    "resource": path,
                    "holder_id": wi_id,
                    "granted_by": wi_id,
                }),
            );
        }
    }

    // Existing LockStrict check (unchanged)
    if bridge.config().strategy.conflict_policy == ConflictPolicy::LockStrict {
        // ... unchanged ...
    }

    // Write file (unchanged)
    // ...
}
```

**Behavior:**

| Scenario | LockAdvisory | LockStrict |
|---|---|---|
| First write to `src/main.py` by Work A | Lock created (informational) | Lock created (blocking) |
| Second write to same file by Work A | Reuses existing lock | Reuses existing lock |
| Write to `src/main.py` by Work B | Second lock created; Coordinator sees contention | Existing LockStrict check blocks the write |

The lock is **idempotent per work_id** — if the same agent writes the same file 10 times, it reuses the first lock. Different agents writing the same file create separate locks, making contention visible.

### Change 2: Lock cleanup on agent exit

In `run_agent_task()`, after the agent loop exits and before worktree cleanup, release all locks held by this agent's `work_id`:

```rust
// Release advisory locks held by this agent
if let Some(ref wi_id) = worktree_key {
    let lock_resp = bridge.request(
        "lock.list",
        serde_json::json!({ "holder_id": wi_id, "active_only": true }),
    );
    if let Some(locks) = lock_resp.result.as_ref().and_then(|v| v.as_array()) {
        for lock in locks {
            if let Some(lock_id) = lock.get("id").and_then(|v| v.as_str()) {
                let _ = bridge.request("lock.release", serde_json::json!({ "id": lock_id }));
            }
        }
        if !locks.is_empty() {
            agent_log.info(&format!("released {} advisory locks for {}", locks.len(), wi_id));
        }
    }
}
```

If the agent process crashes hard (kill -9), locks still have TTL expiry via `config.strategy.max_lock_ttl_minutes`. The `handle_lock_create` handler already auto-expires stale locks on creation.

### How the Coordinator sees contention

`build_state_summary()` in `coordinator.rs` already includes active locks:

```
## Active Locks
- src/main.py: held by wi-abc123
- src/lib.py: held by wi-abc123
- src/main.py: held by wi-def456   ← contention!
```

The Coordinator LLM can see that two Work items are touching `src/main.py` and respond appropriately — e.g., by pausing one agent, re-scoping a Work item, or noting the conflict in a Learning.

### Architecture

```
executor.rs:execute_action() → WriteFile
├── sandbox::validate_sandboxed_path()
├── auto-lock: lock.list → lock.create   ← NEW
├── LockStrict check                     (existing, unchanged)
└── write file

executor.rs:run_agent_task()
├── run_agent_loop() → Result
├── lock cleanup: lock.list → lock.release × N   ← NEW
├── work transition
├── cleanup worktree
└── terminal session status

coordinator.rs:build_state_summary()
├── ... existing sections ...
└── ## Active Locks                      (existing, now populated)
```

### Implementation Plan

| Phase | What | Files |
|-------|------|-------|
| 1 | Auto-lock on `WriteFile` | `executor.rs` |
| 2 | Lock cleanup on agent exit | `executor.rs` |
| 3 | Tests | `executor.rs` |

## Alternatives Considered

### Alternative 1: Explicit `BroadcastTouchedFiles` action

- **Description:** New `AgentAction` variant that agents emit to declare file intentions.
- **Pros:** Decoupled from WriteFile. Agent can declare intent before writing.
- **Cons:** New action type, new handler, new event type. Agents must be prompted to use it. Duplicates what locks already provide.
- **Why not chosen:** The existing lock system already provides resource tracking. Auto-locking on WriteFile gets the same visibility with zero prompt changes.

### Alternative 2: Track touched files on the Bundle record

- **Description:** Populate `Bundle.touched_paths` from WriteFile actions in real-time.
- **Pros:** No new infrastructure — the field already exists.
- **Cons:** Bundle is created at propose-time, not write-time. No real-time visibility during implementation. Two agents on different Works wouldn't see each other's Bundle metadata until propose.
- **Why not chosen:** Too late. Contention needs to be visible during implementation, not after.

### Alternative 3: Event-based broadcasting

- **Description:** Emit a `file.touched` event on every WriteFile; Coordinator subscribes.
- **Pros:** Real-time. No lock overhead.
- **Cons:** Events are fire-and-forget — no persistent record. Coordinator would need to maintain its own in-memory map of who touched what. If Coordinator restarts, map is lost.
- **Why not chosen:** Locks are already persistent records with TTL, query API, and state summary integration. Re-inventing that as events is worse.

### Alternative 4: `resource_tags` matching at assignment time

- **Description:** When assigning an Implementer, the Coordinator checks if the Work's `resource_tags` overlap with any active Work's `resource_tags`.
- **Pros:** Prevents contention before it starts. No runtime overhead.
- **Cons:** `resource_tags` are approximate (set at Work creation time). Agents often touch files not in the tags. False negatives are common.
- **Why not chosen:** Good as a heuristic but insufficient alone. Auto-locks capture actual writes, not predicted ones.

## Technical Considerations

### Dependencies

No new crates. Uses existing `Lock` domain, `lock.create`/`lock.list`/`lock.release` handlers.

### Performance

- `lock.list` by resource: in-memory HashMap scan filtered by `resource + active_only`. Sub-millisecond.
- `lock.create`: HashMap insert + optional TaskStore append. Sub-millisecond for memory, ~1ms for disk.
- Per-WriteFile overhead: one `lock.list` + conditional `lock.create`. Negligible vs. LLM call latency (~3-10s).
- Lock cleanup: one `lock.list` by `holder_id` + N `lock.release`. N < 20 files per agent typically.

### Security

- Under `LockStrict`: auto-locks make the policy actually functional. Previously it was a no-op because nobody acquired locks.
- No new attack surface. Lock creation requires a valid `work_id`.

### Testing Strategy

**Auto-lock tests:**
- WriteFile creates advisory lock with `holder_id = work_id`
- Second WriteFile to same path by same agent reuses existing lock (no duplicate)
- WriteFile by different work_id to same path creates a second lock (visible contention)
- WriteFile without work_id (Coordinator/Researcher) skips lock creation

**Lock cleanup tests:**
- Agent exit releases all locks for its `work_id`
- Agent with no writes has no locks to release (no-op)
- Locks from a crashed agent expire via TTL

**Integration with LockStrict:**
- Agent A locks `src/main.py` via auto-lock
- Agent B's WriteFile to `src/main.py` blocked under LockStrict
- Agent B's WriteFile to `src/main.py` succeeds under LockAdvisory (but lock is visible)

### Rollout Plan

Single commit to `v3` branch. Backward-compatible. No config changes. Under `LockAdvisory` (default), the only visible change is that active locks now appear in the Coordinator's state summary.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Lock accumulation from long-running agents | Low | Low | Cleaned up on exit. TTL expiry as backstop. |
| `lock.list` overhead on hot-path WriteFile | Low | Low | In-memory HashMap scan. Sub-ms. |
| Coordinator overwhelmed by lock info in state summary | Low | Low | Lock section is already in `build_state_summary()`. If too verbose, can truncate or summarize. |
| Auto-lock creates false contention signal (same file in different worktrees) | None | None | Locks are keyed by relative path. Two agents in separate worktrees writing the same relative path IS a real contention signal — they'll conflict at merge. |

## Open Questions

- [ ] Should auto-lock be opt-in (new config flag) or always-on? Current proposal: always-on because it's advisory-only under `LockAdvisory`.
- [ ] Should ReadFile also create (weaker) locks for visibility?
- [ ] Should the Coordinator proactively warn when it sees overlapping locks from different Works?

## References

- Manual test findings: `docs/design/2026-03-01-manual-test-findings.md` (Bug 11)
- Lock domain: `src/domain/lock.rs`
- Lock handlers: `src/daemon/handlers.rs:2375-2600`
- WriteFile LockStrict check: `src/agents/executor.rs:394-409`
- AcquireLock/ReleaseLock: `src/agents/executor.rs:928-988`
- State summary (locks section): `src/agents/coordinator.rs:build_state_summary()`
