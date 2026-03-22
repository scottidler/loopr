# Design Document: Coordinator Override & SLA-Based Work Recovery

**Author:** Scott Idler + Claude
**Date:** 2026-03-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The Coordinator currently operates within the same FSM role constraints as other agents — it cannot force-transition a stuck Work past a state that requires a different role. When an Implementer dies without transitioning Work from `InProgress → InReview`, and the Bundle-aware handback (work-handback doc) also fails or doesn't apply, the entire pipeline deadlocks. This document adds SLA-based timeout detection and Coordinator "override" privileges to forcibly advance or abandon stuck records, closing the last class of pipeline deadlocks.

## Problem Statement

### Background

The existing architecture has multiple recovery layers:
- **Bundle-aware handback** (`work-handback.md`): `run_agent_task()` transitions Work to `InReview` if an active Bundle exists when the agent fails
- **Implementer dedup** (`work-handback.md`): `agent.start` rejects duplicate Implementers on the same Work
- **Coordinator supervisor** (`live-run-fixes.md`): Daemon restarts the Coordinator with exponential backoff on failure
- **Lifeguard** (`agent-runtime-bugs.md`): Detects futile loops and escalates to `NeedHelp`

These address the *common* failure paths. But edge cases remain where no actor has the authority to advance the Work FSM.

### Problem

**Scenario 1 — Handback race:** Agent fails. `run_agent_task()` runs `determine_work_handback()`, which reads Bundles. Between the read and the transition call, the Bundle is rejected by a concurrent Reviewer. The handback transitions Work → `InReview`, but the only Bundle is now Rejected. Work is stuck in `InReview` with no valid Bundle. Nobody transitions it back to `Ready` because `InReview → Ready` doesn't exist.

**Scenario 2 — Agent crash without handback:** A tokio task panics (e.g., LLM client OOM). The task is aborted by the runtime. `run_agent_task()`'s post-loop logic never runs. The session goes to `Failed` (via Drop or abort handler), but the Work remains in `InProgress`. The Coordinator sees it as "assigned" and waits. Indefinitely.

**Scenario 3 — SLA breach without loop detection:** An Implementer writes different code each iteration (no repeated actions for lifeguard to detect) but never converges on passing tests. 20 iterations burn. The Work goes to `Blocked` via handback, Coordinator re-assigns, another 20 iterations burn. The Work has been in-progress for an hour with no useful output. Nothing detects this.

**Common root cause:** The Work FSM enforces strict role-based transitions. `InProgress → InReview` requires `Role::Implementer`. `InReview → Integrated` requires `Role::Integrator`. The Coordinator (with `Role::Coordinator`) cannot push a stuck Work through these gates. There is no "manager override" path.

### Goals

- Coordinator can forcibly transition any Work through `Override` transitions when an SLA is breached
- SLA detection is time-based (wall-clock) and attempt-based (re-assignment count)
- Override transitions are auditable (logged with reason, visible in TUI)
- Stuck Work can be: abandoned, force-advanced to InReview (if Bundle exists), or reset to Ready
- Zero changes to the normal-path FSM — overrides are exceptional

### Non-Goals

- Arbitrary FSM override for non-Work records (Plans/Specs/Phases have no stuck-state problem)
- Automatic override without Coordinator judgment (the LLM decides whether to override)
- Override without audit trail
- Changing the deterministic Integrator's logic
- Cross-phase override (if all Works in a Phase are stuck, that's a different problem — the Phase should be re-planned)

## Proposed Solution

### Overview

Three additions:
1. **Override transitions in the Work FSM** — New `override_*` transition rules that bypass role constraints, gated by `Role::Coordinator` + an `override: true` flag
2. **SLA tracking in CoordinatorState** — Wall-clock timestamp + attempt count per Work, surfaced in Coordinator context
3. **`OverrideWork` Coordinator action** — New action type that triggers override transitions with an audit reason

### Architecture

```
CoordinatorState.work_attempts: HashMap<String, u32>     (existing)
CoordinatorState.work_first_assigned_at: HashMap<String, i64>  ← NEW

Coordinator context (build_state_summary):
  "Work wi-123: InProgress, attempts: 3/3, age: 47min/30min (SLA BREACHED)"

Coordinator action:
  OverrideWork { work_id, target_status, reason }

Work FSM:
  InProgress → Ready        [Coordinator + override]
  InProgress → InReview     [Coordinator + override]
  InProgress → Abandoned    [Coordinator + override]
  InReview   → Ready        [Coordinator + override]
  InReview   → Abandoned    [Coordinator + override]
  Blocked    → Ready        [Coordinator + override]   (already exists without override)
  Blocked    → Abandoned    [Coordinator + override]
```

### Change 1: Override Transitions in Work FSM

**File:** `src/domain/work.rs`

Add a new `override_transitions()` function that defines Coordinator-only escape hatches:

```rust
/// Override transitions available only to the Coordinator with the override flag.
/// These bypass normal role constraints for recovery from stuck states.
fn override_transitions() -> Vec<FsmTransition<WorkStatus>> {
    vec![
        // Reset stuck InProgress back to Ready for re-assignment
        FsmTransition {
            from: WorkStatus::InProgress,
            to: WorkStatus::Ready,
            allowed_roles: vec![Role::Coordinator],
        },
        // Force-advance InProgress to InReview when a valid Bundle exists
        FsmTransition {
            from: WorkStatus::InProgress,
            to: WorkStatus::InReview,
            allowed_roles: vec![Role::Coordinator],
        },
        // Abandon stuck InProgress
        FsmTransition {
            from: WorkStatus::InProgress,
            to: WorkStatus::Abandoned,
            allowed_roles: vec![Role::Coordinator],
        },
        // Reset InReview back to Ready (no valid Bundle)
        FsmTransition {
            from: WorkStatus::InReview,
            to: WorkStatus::Ready,
            allowed_roles: vec![Role::Coordinator],
        },
        // Abandon stuck InReview
        FsmTransition {
            from: WorkStatus::InReview,
            to: WorkStatus::Abandoned,
            allowed_roles: vec![Role::Coordinator],
        },
        // Abandon stuck Blocked
        FsmTransition {
            from: WorkStatus::Blocked,
            to: WorkStatus::Abandoned,
            allowed_roles: vec![Role::Coordinator],
        },
    ]
}
```

**Transition dispatch change in `handlers.rs`:**

The `work.transition` handler currently calls `work_transitions()` to validate. Add a `override` boolean parameter:

```rust
fn handle_work_transition(stores: &Stores, req: DaemonRequest) -> DaemonResponse {
    let id = req.params["id"].as_str().unwrap();
    let target = req.params["target_status"].as_str().unwrap();
    let role = parse_role(&req.params["role"]);
    let is_override = req.params.get("override").and_then(|v| v.as_bool()).unwrap_or(false);

    let transitions = if is_override {
        override_transitions()
    } else {
        work_transitions()
    };

    // ... existing validation against `transitions` ...

    if is_override {
        let reason = req.params.get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("no reason provided");
        log::warn!(
            "OVERRIDE: Work {} transitioned {} → {} by Coordinator (reason: {})",
            id, current_status, target, reason
        );
        // Emit auditable event
        event_tx.send(DaemonEvent::new(
            "work.override_transition",
            json!({ "work_id": id, "from": current_status, "to": target, "reason": reason }),
        ));
    }

    // ... existing transition logic ...
}
```

**Key design decision: separate transition table.** Override transitions are NOT mixed into `work_transitions()`. They are a separate table checked only when `override: true`. This means:
- Normal agents (Implementer, Reviewer) can never trigger overrides even if they set `override: true` — the table only allows `Role::Coordinator`
- The normal FSM is unchanged — no risk of accidental override
- Code review can audit override transitions in isolation

### Change 2: SLA Tracking in CoordinatorState

**File:** `src/domain/coordinator_state.rs`

Add wall-clock tracking per Work:

```rust
pub struct CoordinatorState {
    // ... existing fields ...
    pub work_attempts: HashMap<String, u32>,           // existing
    pub work_first_assigned_at: HashMap<String, i64>,  // NEW: millis since epoch
}
```

**SLA config:**

```rust
// In StrategyConfig (src/config.rs)
pub struct WorkSlaConfig {
    pub max_attempts: u32,              // default 3
    pub max_wall_clock_minutes: u64,    // default 30
}
```

**Where `work_first_assigned_at` is set:** In `handle_work_transition()` when a Work transitions to `InProgress` — NOT in the `AssignAgent` executor. This ensures ALL paths that start a Work are tracked (AssignAgent, auto_start_agents, manual CLI, pull-based workers). The handler checks if the entry already exists; if not, it sets it to `now_millis()`. If the entry exists (re-assignment after Blocked → Ready → InProgress), it is NOT overwritten — the SLA tracks time since the FIRST assignment.

**SLA detection in `build_state_summary()`:**

When building the Work status list in the Coordinator's context, annotate each Work with SLA status:

```rust
fn format_work_sla(
    work: &Work,
    coord_state: &CoordinatorState,
    sla: &WorkSlaConfig,
    now: i64,
) -> String {
    let attempts = coord_state.attempts(&work.id);
    let age_minutes = coord_state.work_first_assigned_at
        .get(&work.id)
        .map(|&started| (now - started) / 60_000)
        .unwrap_or(0);

    let attempt_breach = attempts >= sla.max_attempts;
    let time_breach = age_minutes >= sla.max_wall_clock_minutes as i64;

    if attempt_breach || time_breach {
        format!(
            "**SLA BREACHED** — attempts: {}/{}, age: {}min/{}min. Consider using override_work to abandon or reset.",
            attempts, sla.max_attempts, age_minutes, sla.max_wall_clock_minutes
        )
    } else {
        format!(
            "attempts: {}/{}, age: {}min/{}min",
            attempts, sla.max_attempts, age_minutes, sla.max_wall_clock_minutes
        )
    }
}
```

The "**SLA BREACHED**" annotation in the Coordinator's context is sufficient for the LLM to decide to override. We don't auto-override because the Coordinator may have better judgment (e.g., "this Work is almost done, give it one more try" vs. "this Work is fundamentally flawed, abandon it").

### Change 3: `OverrideWork` Coordinator Action

**File:** `src/agents/mod.rs`, `src/agents/executor.rs`, `src/agents/coordinator.rs`

New action variant:

```rust
// In AgentAction enum
OverrideWork {
    work_id: String,
    target_status: String,  // "ready", "in_review", "abandoned"
    reason: String,
}
```

Executor implementation:

```rust
AgentAction::OverrideWork { work_id, target_status, reason } => {
    // Kill any active Implementer sessions on this Work
    let sessions = stores.agent_sessions.read().unwrap();
    let active_sessions: Vec<String> = sessions.values()
        .filter(|s| s.work_id.as_deref() == Some(&work_id) && !s.status.is_terminal())
        .map(|s| s.id.clone())
        .collect();
    drop(sessions);

    for sid in &active_sessions {
        bridge.request("agent.stop", json!({ "session_id": sid }))?;
    }

    // Override transition
    let resp = bridge.request("work.transition", json!({
        "id": work_id,
        "target_status": target_status,
        "role": "coordinator",
        "override": true,
        "reason": reason,
    }));

    // Release any locks held by this Work
    let locks = stores.locks.read().unwrap();
    let work_locks: Vec<String> = locks.values()
        .filter(|l| l.holder_id == work_id && l.status == LockStatus::Active)
        .map(|l| l.id.clone())
        .collect();
    drop(locks);

    for lid in &work_locks {
        bridge.request("lock.release", json!({ "lock_id": lid }))?;
    }

    // Create audit Learning
    bridge.request("learning.create", json!({
        "content": format!("Override: Work {} → {} (reason: {})", work_id, target_status, reason),
        "scope": "work",
        "source_id": work_id,
        "applicable_roles": ["coordinator"],
        "resource_tags": ["override", "audit"],
    }))?;

    resp.into()
}
```

**Race condition: override vs. agent completion.** The `OverrideWork` executor calls `agent.stop` which sets the session to `Cancelled`. The agent checks cancellation at iteration boundaries (not mid-action). If the agent completes between the stop call and the override transition, the Work may already be in a terminal state. The override transition will fail (Work already `InReview` or `Done`), which is fine — the Work recovered on its own. The executor should handle this gracefully (log the transition failure as info, not error).

**Coordinator prompt addition:**

```
16. `override_work` — Force-transition a stuck Work (SLA breached only)
    {"action": "override_work", "work_id": "...", "target_status": "ready|abandoned", "reason": "..."}
    USE ONLY when SLA is breached. Options:
    - "ready": Reset for re-assignment (kills active agents, releases locks)
    - "abandoned": Give up on this Work (blocks dependent Works; consider re-planning)
    - "in_review": Force to review if a valid Bundle exists
```

### Data Model

| Change | Type | File |
|--------|------|------|
| `CoordinatorState.work_first_assigned_at` | New field (`HashMap<String, i64>`) | `coordinator_state.rs` |
| `WorkSlaConfig` | New config struct | `config.rs` |
| `StrategyConfig.work_sla` | New config field | `config.rs` |
| `override_transitions()` | New function | `work.rs` |
| `OverrideWork` | New action variant | `mod.rs` |
| `work.override_transition` | New event type | `handlers.rs` |

### Implementation Plan

**Phase 1: Override transitions**
- Add `override_transitions()` to `work.rs`
- Add `override` parameter handling to `handle_work_transition()` in `handlers.rs`
- Add audit event emission
- Tests: override transitions succeed for Coordinator, reject for other roles, reject without override flag

**Phase 2: SLA tracking**
- Add `work_first_assigned_at` to `CoordinatorState`
- Add `WorkSlaConfig` to `StrategyConfig`
- Wire `work_first_assigned_at` recording into `AssignAgent` executor
- Add SLA annotation to `build_state_summary()`
- Tests: SLA breach detection (time-based, attempt-based, both), SLA annotation formatting

**Phase 3: OverrideWork action**
- Add `OverrideWork` to `AgentAction`
- Implement executor logic (kill agents, transition, release locks, audit)
- Add to Coordinator prompt
- Tests: override kills agents, transitions Work, releases locks, creates audit Learning

## Alternatives Considered

### Alternative 1: Automatic override (no LLM judgment)

- **Description:** Daemon-level task that auto-overrides any Work past SLA, no Coordinator involvement
- **Pros:** Deterministic, no LLM cost. Guaranteed to act.
- **Cons:** No judgment. A Work at 31 minutes that's on its last iteration and about to succeed gets killed. No ability to say "give it 5 more minutes."
- **Why not chosen:** The Coordinator is the PM. It should decide when to cut losses vs. persist. The SLA annotation gives it the information; the override action gives it the power.

### Alternative 2: Relax the Work FSM to allow Coordinator for all transitions

- **Description:** Add `Role::Coordinator` to every transition in `work_transitions()`
- **Pros:** Simplest change. No new action, no override flag.
- **Cons:** Destroys the role-based safety guarantees. A Coordinator LLM bug could skip InReview, bypass the Reviewer, and push code to Done. The whole point of the FSM is to prevent this.
- **Why not chosen:** Role constraints on the normal path are a feature, not a bug. Overrides must be explicit and auditable.

### Alternative 3: System role with full privileges

- **Description:** Add a `Role::System` that can make any transition, used only by daemon internals
- **Pros:** Clean separation. No Coordinator involvement.
- **Cons:** System role bypasses all guards silently. No audit trail from agent context. Harder to debug "why did this Work suddenly become Ready?"
- **Why not chosen:** Override should be a Coordinator decision, not a daemon-internal mechanism. The Coordinator creates an audit Learning explaining why.

## Technical Considerations

### Dependencies

No new crates. Uses existing `serde_json`, `log`.

### Performance

- SLA check: one HashMap lookup per Work during context build. Negligible.
- Override transitions: one additional table lookup. Negligible.

### Security

Override transitions are auditable via:
1. `log::warn!` in the handler (daemon logs)
2. `work.override_transition` event (TUI visibility)
3. Audit Learning record (TaskStore persistence)

### Testing Strategy

**Unit tests:**
- `work.rs`: Override transitions valid for Coordinator, rejected for other roles
- `work.rs`: Override transitions rejected when `override` flag is false
- `coordinator_state.rs`: `work_first_assigned_at` tracking, SLA breach detection
- `config.rs`: `WorkSlaConfig` deserialization with defaults

**Integration tests:**
- Override kills active Implementer session before transitioning
- Override releases locks held by the Work
- Override creates audit Learning with reason
- Coordinator context shows "SLA BREACHED" annotation when threshold exceeded

### Rollout Plan

Backward-compatible. New `work_first_assigned_at` field defaults to empty HashMap. New config field defaults to `max_attempts: 3, max_wall_clock_minutes: 30`. Override transitions are additive — existing FSM is unchanged.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coordinator overrides too aggressively (abandons recoverable Work) | Medium | Medium | SLA defaults are generous (3 attempts, 30 min). Prompt guidance says "USE ONLY when SLA is breached." Audit trail enables post-mortem. |
| Override leaves orphaned worktree | Low | Low | `OverrideWork` executor kills agent sessions, which triggers `run_agent_task()` cleanup. Worktree cleanup is best-effort. |
| Race: override and agent completion happen simultaneously | Low | Medium | Override kills agent first, then transitions. If agent already completed, kill is a no-op. Transition may fail (Work already in terminal state) — that's fine. |
| `work_first_assigned_at` not set for pre-existing Works (migration) | Low | Low | Defaults to empty HashMap. Missing entries treated as "no SLA tracking" (infinite time, 0 attempts). |

## Open Questions

- [ ] Should the SLA thresholds be per-Work (based on estimated complexity) or global?
- [ ] Should `OverrideWork` with `target_status: "in_review"` verify that an active Bundle exists, or trust the Coordinator's judgment? (If no Bundle exists, the Work will be stuck in InReview again.)
- [ ] Should overrides be rate-limited (max 1 override per Coordinator iteration) to prevent override storms?
- [ ] Should the TUI show a special indicator for overridden Works?
- [ ] Should `work_first_assigned_at` be cleared when a Work is overridden to `Ready` (resetting the SLA clock for the new attempt)?

## References

- `docs/design/2026-03-01-work-handback.md` — Bundle-aware handback (addresses the common case)
- `docs/design/2026-03-01-live-run-fixes.md` — Coordinator supervisor (restart on failure)
- `docs/design/2026-03-01-manual-test-findings.md` — Bug 10 (Work deadlock), Bug 5 (duplicate assignment)
- `src/domain/work.rs` — Work FSM transitions
- `src/domain/coordinator_state.rs` — CoordinatorState with work_attempts
- `src/agents/coordinator.rs` — Coordinator agent loop
- `src/daemon/handlers.rs` — work.transition handler
- `docs/design/2026-03-01-pull-based-work-queue.md` — Pull-based workers (interacts: overridden Work reset to Ready is immediately pullable)
- `docs/design/2026-03-01-agent-self-correction-loop.md` — Self-correction loop (interacts: fewer SLA breaches when agents self-correct)
