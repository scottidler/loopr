# Design Document: Work Handback and Implementer Dedup

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Manual E2E testing revealed two interrelated agent runtime problems: (1) an Implementer that hits `max_iterations` leaves its Work stuck in `InProgress` even when it produced a usable Bundle, creating a deadlock; (2) two implementers get assigned to the same Work because `auto_start_agents()` and `AssignAgent` both fire, causing worktree races. This document fixes both.

## Problem Statement

### Background

The Work status FSM enforces role-based transitions:
- `InProgress → InReview`: only `Role::Implementer`
- `InReview → Integrated`: only `Role::Integrator`
- `Integrated → Done`: `Role::Coordinator` or `Role::Integrator`

The `agent.start` handler has Researcher dedup (Gap #26) that prevents duplicate Researchers on the same `target_id`. No equivalent guard exists for Implementers on the same `work_id`.

An `auto_start_agents()` hook in the dispatcher (Gap #29, `handlers.rs:211-249`) auto-spawns an implementer whenever any `work.transition → InProgress` occurs — including transitions triggered *internally* by `AssignAgent`.

### Problem

**Bug 10 — Work deadlock after agent failure:**

1. Coordinator assigns Implementer → Work → `InProgress`
2. Implementer writes code, proposes Bundle → `Proposed`
3. Bundle progresses: Triaged → Reviewed → `Accepted`
4. Implementer hits `max_iterations` → `run()` returns `Err`
5. Post-loop logic blindly transitions Work → `Blocked`
6. Coordinator re-assigns → Work → `InProgress` — but nobody can do `InProgress → InReview`
7. **Deadlock**: accepted code, no path to Done.

Root cause: post-loop transition is Bundle-blind.

**Bug 5 — Duplicate implementer on same Work:**

1. Coordinator calls `AssignAgent { "implementer", work_id }`
2. Executor calls `work.transition(InProgress)` as a pre-step
3. `auto_start_agents()` hook fires → `agent.start` → implementer #1
4. `AssignAgent` handler then explicitly calls `agent.start` → implementer #2
5. Both share the same worktree path, racing on file writes

Root cause: `auto_start_agents()` and `AssignAgent` both spawn. No dedup in `agent.start` for implementers.

### Goals

- Eliminate the Work deadlock (Bundle-aware handback)
- Prevent duplicate implementers on the same Work (dedup guard)
- Minimal, loosely-coupled changes

### Non-Goals

- Redesigning the Work/Bundle FSMs
- File-touch broadcasting / resource contention signaling (separate doc)
- Cross-agent file merge resolution
- Changing `max_iterations` behavior

## Proposed Solution

### Overview

Two changes:

1. **Implementer dedup in `agent.start`** — reject if a non-terminal implementer session already exists for the same `work_id`
2. **Bundle-aware post-loop handback** — inspect Bundle state before choosing the Work transition

### Change 1: Implementer dedup in `agent.start` handler

Mirror the existing Researcher dedup (Gap #26, `handlers.rs:3404-3420`). In `handle_agent_start()`, after the `max_pool` check and inside the existing sessions read lock:

```rust
// Implementer dedup by work_id (mirrors Gap #26 Researcher dedup by target_id)
if agent_type == AgentType::Implementer {
    if let Some(ref wi_id) = work_id {
        let has_existing = sessions.values().any(|s| {
            s.agent_type == AgentType::Implementer
                && !s.status.is_terminal()
                && s.work_id.as_deref() == Some(wi_id)
        });
        if has_existing {
            return DaemonResponse::err(
                req.id,
                RpcError::precondition_failed(&format!(
                    "non-terminal Implementer session already exists for work_id '{}'",
                    wi_id
                )),
            );
        }
    }
}
```

This blocks duplicates from **all** sources — `AssignAgent`, `auto_start_agents()`, manual CLI — because they all funnel through `agent.start`.

The `auto_start_agents()` hook does NOT need changes. It calls `agent.start`, which now rejects the duplicate. The first spawn wins; the second gets a `precondition_failed` error that is silently ignored (the hook already discards the response with `let _ = dispatch(...)`).

### Change 2: Bundle-aware handback in `run_agent_task()`

Replace the binary Ok/Err check with:

```rust
fn determine_work_handback(
    stores: &Stores,
    work_id: &str,
    session_id: &str,
    succeeded: bool,
) -> Option<&'static str> {
    if succeeded {
        return Some("InReview");
    }

    // If a sibling implementer is still active, don't touch the Work.
    let sessions = stores.agent_sessions.read().unwrap();
    let sibling_active = sessions.values().any(|s| {
        s.id != session_id
            && s.agent_type == AgentType::Implementer
            && s.work_id.as_deref() == Some(work_id)
            && !s.status.is_terminal()
    });
    drop(sessions);

    if sibling_active {
        return None; // let the sibling finish
    }

    // Did the agent produce a usable Bundle?
    let bundles = stores.bundles.read().unwrap();
    let has_active_bundle = bundles.values().any(|b| {
        b.work_id == work_id
            && !matches!(b.status, BundleStatus::Rejected | BundleStatus::Superseded)
    });

    Some(if has_active_bundle { "InReview" } else { "Blocked" })
}
```

Wire into `run_agent_task()`:
```rust
if agent_type == AgentType::Implementer {
    if let Some(ref wi_id) = worktree_key {
        if let Some(target) = determine_work_handback(&stores, wi_id, &session_id, result.is_ok()) {
            let resp = bridge.request(
                "work.transition",
                serde_json::json!({ "id": wi_id, "target_status": target, "role": "implementer" }),
            );
            // ... logging ...
        }
    }
}
```

**Decision table:**

| Agent result | Sibling active? | Bundle state | Work transition |
|---|---|---|---|
| Ok | — | — | `InReview` |
| Err | Yes | — | (skip) |
| Err | No | active Bundle | `InReview` |
| Err | No | all Rejected/none | `Blocked` |

### Architecture

```
handlers.rs:handle_agent_start()
├── max_pool check
├── Researcher dedup by target_id     (existing, Gap #26)
├── Implementer dedup by work_id      ← NEW (Change 1)
└── create session + spawn task

executor.rs:run_agent_task()
├── run_agent_loop() → Result
├── determine_work_handback()          ← NEW (Change 2)
│   ├── Ok → "InReview"
│   ├── Err + sibling → skip
│   ├── Err + active Bundle → "InReview"
│   └── Err + no Bundle → "Blocked"
├── work.transition (if not skipped)
├── cleanup worktree
└── terminal session status
```

### Implementation Plan

| Phase | What | Files |
|-------|------|-------|
| 1 | Implementer dedup in `agent.start` | `handlers.rs` |
| 2 | `determine_work_handback()` + wire into `run_agent_task()` | `executor.rs` |
| 3 | Tests | `handlers.rs`, `executor.rs` |

## Alternatives Considered

### Alternative 1: Suppress `auto_start_agents()` during `AssignAgent`

- **Description:** Pass a flag through the dispatcher to skip the auto-start hook when the transition originated from `AssignAgent`.
- **Pros:** Targeted — only suppresses the specific double-spawn path.
- **Cons:** Threading a flag through the dispatcher is invasive. Doesn't prevent duplicates from other sources (e.g., two `AssignAgent` calls in the same LLM response, or manual CLI).
- **Why not chosen:** The dedup guard in `agent.start` is defense-in-depth — blocks ALL duplicate sources, not just auto-start.

### Alternative 2: Remove `auto_start_agents()` entirely

- **Description:** Delete the hook and let the Coordinator be the sole agent spawner.
- **Pros:** Simplest fix for Bug 5. One code path for agent creation.
- **Cons:** Breaks the non-Coordinator flow where the CLI or TUI transitions work manually. The hook exists so that `loopr work transition wi-123 InProgress` auto-starts an implementer without needing the Coordinator.
- **Why not chosen:** The hook serves a valid purpose for manual/CLI-driven workflows.

### Alternative 3: Add `InProgress → Done` for Coordinator

- **Description:** Let the Coordinator skip the review pipeline.
- **Pros:** Simple — one transition rule.
- **Cons:** Bypasses Integrator validation. Unvalidated code reaches `Done`.
- **Why not chosen:** Undermines the review/integration pipeline.

### Alternative 4: Auto-transition Work on Bundle acceptance

- **Description:** `accept_bundle` handler auto-transitions parent Work → `InReview`.
- **Pros:** Reactive — happens exactly when Bundle is accepted.
- **Cons:** Handler runs in Coordinator role context, but `InProgress → InReview` requires `Role::Implementer`. Would need a system role or role override.
- **Why not chosen:** The handback in `run_agent_task()` runs in Implementer context naturally.

## Technical Considerations

### Dependencies

No new crates. Uses existing `BundleStatus`, `AgentSession`.

### Performance

- Dedup guard: O(n) scan of sessions; n < 20 (global cap). Runs once per `agent.start`.
- `determine_work_handback()`: reads sessions + bundles; both in-memory HashMaps. Microseconds.

### Security

Dedup guard prevents resource waste (duplicate LLM calls). No new attack surface.

### Testing Strategy

**Dedup guard tests:**
- `agent.start` rejects second implementer on same work_id
- `agent.start` allows after first session reaches terminal state
- `auto_start_agents()` + `AssignAgent` → only one session created

**Handback tests:**
- Agent succeeded → "InReview"
- Agent failed + active Bundle → "InReview"
- Agent failed + all Rejected → "Blocked"
- Agent failed + no Bundles → "Blocked"
- Agent failed + sibling active → None (skip)

### Rollout Plan

Single commit to `v3` branch. Backward-compatible. No schema changes.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Dedup rejects legitimate re-assignment after failure | Low | Med | Guard checks `!is_terminal()`. Failed/Completed sessions pass. Coordinator's retry flow: agent fails → session terminal → re-assign succeeds. |
| Race between sibling check and sibling death | Low | Low | Worst case: both skip. Coordinator re-assigns next iteration. |
| Bundle rejected between agent death and handback check | Low | Low | Bundle query checks current status. If all Bundles rejected, Work correctly goes to Blocked. |

## Open Questions

- [ ] Should the dedup guard also apply to Reviewer sessions on the same `bundle_id`?

## References

- Manual test findings: `docs/design/2026-03-01-manual-test-findings.md` (Bugs 5 and 10)
- Work FSM: `src/domain/work.rs:29-108`
- Bundle FSM: `src/domain/bundle.rs:29-100`
- Post-loop logic: `src/agents/executor.rs:177-198`
- Auto-start hook: `src/daemon/handlers.rs:211-249`
- Researcher dedup: `src/daemon/handlers.rs:3404-3420`
