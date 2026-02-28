# Design Document: MVP4 Post-Build Validation & Fixes

**Author:** Claude + Scott
**Date:** 2026-02-27
**Status:** Draft
**Review Passes:** 2/5

## Summary

MVP4 was built by the RWL across 13 iterations. `otto ci` passes with 1058 tests. Deep inspection reveals 4 issues where code compiles and tests pass but doesn't actually work at runtime. This document specifies the fixes and a comprehensive e2e validation test suite.

## Problem Statement

### Background

MVP4 adds Coordinator, Researcher, Integrator agents plus ContextBuilder, strategy knobs, and advisory locks. The RWL built it incrementally — each iteration did one unit of work, committed, and validated with `otto ci`. This is effective for getting code to compile and pass unit tests but misses integration-level correctness.

### Problem

Post-build inspection found:

1. **8 Coordinator actions return `NotYetImplemented`** — The executor stubs CreatePlan/CreateSpec/CreatePhase/CreateWork/AssignAgent/SpawnResearcher/TriageBundle/AcceptBundle even though the daemon handlers exist. The Coordinator LLM loop will call these, get stub responses, and log "stub(create_plan): ..." instead of actually creating records.

2. **`coordinator.get_goal` handler missing** — CLI `loopr coordinator status` maps to `"coordinator.get_goal"` but no handler exists in the dispatch table. This will return "method not found" at runtime.

3. **Advisory lock checking not enforced in WriteFile** — `ConflictPolicy::LockStrict` is defined but `WriteFile` in executor.rs has no lock checking. Under strict policy, agents should be blocked from writing to locked resources.

4. **TUI agents view doesn't show target_id/query** — For Researcher/Coordinator/Integrator agents (which lack work_id/bundle_id), the display shows nothing for the target column.

### Goals

- Wire all 8 Coordinator actions to real bridge calls
- Add missing `coordinator.get_goal` handler
- Enforce advisory lock checking under `LockStrict` in WriteFile
- Fix TUI agents view for thinking-plane agents
- Add comprehensive e2e integration tests that exercise the full MVP4 pipeline
- Remove `NotYetImplemented` scaffolding variant from `ActionResult`

### Non-Goals

- Changing Coordinator system prompts or generation logic
- Adding new agent types or actions beyond what exists
- Performance optimization
- Changing the existing Implementer/Reviewer agent flows

## Proposed Solution

### Overview

Wire the 8 stubbed actions in `executor.rs` using the same `bridge.request()` pattern as existing actions (AcquireLock, ValidateDocument). Add the missing handler. Add lock checking. Add e2e tests.

### Fix 1: Wire Coordinator Actions (`src/agents/executor.rs`)

Add two new `ActionResult` variants:
```rust
RecordCreated { collection: String, id: String },
AgentSpawned { session_id: String, agent_type: String },
```

Wire each action:

| Action | Bridge Method | Params | Result |
|--------|--------------|--------|--------|
| `CreatePlan` | `plan.create` | `{ title, description, acceptance_criteria }` | `RecordCreated { "plans", id }` |
| `CreateSpec` | `spec.create` | `{ plan_id, title, description }` | `RecordCreated { "specs", id }` |
| `CreatePhase` | `phase.create` | `{ spec_id, title, description, order }` | `RecordCreated { "phases", id }` |
| `CreateWork` | `work.create` | `{ phase_id, title, description }` | `RecordCreated { "works", id }` |
| `AssignAgent` | `agent.start` | `{ agent_type, work_id/bundle_id/target_id }` | `AgentSpawned { session_id, agent_type }` |
| `SpawnResearcher` | `agent.start` | `{ agent_type: "researcher", target_id: scope_id, query }` | `AgentSpawned { session_id, "researcher" }` |
| `TriageBundle` | `bundle.transition` | `{ id, target_status: "Triaged", role: "coordinator" }` | `Transitioned(...)` |
| `AcceptBundle` | `bundle.transition` | `{ id, target_status: "Accepted", role: "coordinator" }` | `Transitioned(...)` |

For `AssignAgent`, the `target_id` field from the Coordinator LLM maps to:
- `work_id` for Implementer
- `bundle_id` for Reviewer
- `target_id` for Researcher/Coordinator/Integrator

Pattern (follows existing `AcquireLock`):
```rust
AgentAction::CreatePlan { title, description, acceptance_criteria } => {
    let resp = bridge.request("plan.create", json!({
        "title": title, "description": description,
        "acceptance_criteria": acceptance_criteria,
    }));
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone())
            .unwrap_or_else(|| "unknown error".to_string());
        return Ok(ActionResult::ActionError(format!("create_plan failed: {}", msg)));
    }
    let id = resp.result.as_ref()
        .and_then(|v| v.get("id")).and_then(|v| v.as_str())
        .unwrap_or("unknown").to_string();
    Ok(ActionResult::RecordCreated { collection: "plans".to_string(), id })
}
```

Remove `ActionResult::NotYetImplemented` variant entirely. Update ALL match arms that reference it:
- `coordinator.rs:564` — `format_action_summary()` match arm
- `implementer.rs:312` — inline format match arm (has its own copy)
- `executor.rs:619` — variant definition

Update `format_action_summary()` in `coordinator.rs` and `implementer.rs`:
```rust
ActionResult::RecordCreated { collection, id } => format!("created {}: {}", collection, id),
ActionResult::AgentSpawned { session_id, agent_type } => format!("spawned {} ({})", agent_type, session_id),
```

### Fix 2: Add `coordinator.get_goal` Handler (`src/daemon/handlers.rs`)

Add to dispatch table:
```rust
"coordinator.get_goal" => handle_coordinator_get_goal(stores, req),
```

Handler implementation:
```rust
fn handle_coordinator_get_goal(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let goals = stores.coordinator_goals.read().unwrap();
    let active = goals.values().find(|g| g.active);
    match active {
        Some(goal) => {
            let json = serde_json::to_value(goal).unwrap_or(json!(null));
            DaemonResponse::ok(req.id, json)
        }
        None => DaemonResponse::ok(req.id, json!({ "active": false, "message": "no active goal" })),
    }
}
```

### Fix 3: Lock Checking in WriteFile (`src/agents/executor.rs`)

The executor already receives `bridge` (which has access to stores/config). Add a lock check before writing when policy is `LockStrict`:

```rust
AgentAction::WriteFile { path, content } => {
    // ... existing path sandbox validation ...

    // Advisory lock check (LockStrict only)
    if bridge.config().strategy.conflict_policy == ConflictPolicy::LockStrict {
        let resp = bridge.request("lock.list", json!({ "resource": path, "active_only": true }));
        if let Some(locks) = resp.result.as_ref().and_then(|v| v.as_array()) {
            if !locks.is_empty() {
                let holder = locks[0].get("holder_id").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Ok(ActionResult::ActionError(format!(
                    "file '{}' is locked by {} (policy: LockStrict)", path, holder
                )));
            }
        }
    }

    // ... existing write logic ...
}
```

Need: Add `pub fn config(&self) -> &Config` method to `AgentIpcBridge` (it already stores `config: Config`).

### Fix 4: TUI Agents View (`src/tui/views/agents.rs`)

Update the target display logic at line 28-31:
```rust
let target = match (&session.work_id, &session.bundle_id, &session.target_id, &session.query) {
    (Some(wi), _, _, _) => format!(" wi:{}", &wi[..wi.len().min(8)]),
    (_, Some(b), _, _) => format!(" b:{}", &b[..b.len().min(8)]),
    (_, _, Some(t), Some(q)) => format!(" {}:{}", &t[..t.len().min(8)], truncate(q, 20)),
    (_, _, Some(t), None) => format!(" target:{}", &t[..t.len().min(8)]),
    (_, _, None, Some(q)) => format!(" q:{}", truncate(q, 20)),
    _ => String::new(),
};
```

### E2E Integration Tests (`src/integration_tests.rs`)

Add tests that exercise the real action execution path:

1. **`test_coordinator_action_creates_plan`** — Execute `CreatePlan` action through `execute_action()`, verify plan exists in stores.

2. **`test_coordinator_creates_full_hierarchy`** — CreatePlan → CreateSpec → CreatePhase → CreateWork through executor, verify all records exist with correct parent linkage.

3. **`test_coordinator_assigns_implementer`** — Create hierarchy → AssignAgent(implementer, work_id) → verify session created with correct work_id.

4. **`test_coordinator_spawns_researcher`** — SpawnResearcher(query, scope_id) → verify session created with agent_type=Researcher, correct query and target_id.

5. **`test_coordinator_triage_accept_bundle`** — Create hierarchy + bundle → TriageBundle → verify Triaged. Then simulate review → AcceptBundle → verify Accepted.

6. **`test_write_file_lock_strict_blocks`** — Acquire lock on resource → attempt WriteFile under LockStrict → verify ActionError returned.

7. **`test_write_file_lock_advisory_allows`** — Acquire lock on resource → attempt WriteFile under LockAdvisory → verify write succeeds.

8. **`test_coordinator_get_goal_handler`** — Set goal → get goal → verify response. Clear goal → get goal → verify "no active goal".

9. **`test_full_mvp4_pipeline`** — Set goal → create Plan → create Spec → create Phase → create Work → assign Implementer → propose Bundle → triage → review → accept. Verify the complete chain of records exists.

## Technical Considerations

### Dependencies

No new dependencies needed. All bridge/handler infrastructure exists.

### Testing Strategy

All new integration tests use the existing `dispatch()` or `execute_action()` pattern with `test_stores()` setup. Agent spawning tests need `#[tokio::test]` since `execute_action` is async.

### Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| AssignAgent param mapping wrong for implementer/reviewer | Medium | High | Test each agent type assignment separately |
| Lock path matching too literal (exact string vs prefix) | Low | Medium | Document: locks match on exact resource string |
| Removing NotYetImplemented breaks test assertions | Low | Low | Update all test assertions in same change |

## Files to Modify

| File | Change |
|------|--------|
| `src/agents/executor.rs` | Wire 8 actions, add ActionResult variants, add lock check, remove NotYetImplemented |
| `src/agents/coordinator.rs` | Update format_action_summary, update tests |
| `src/agents/bridge.rs` | Add `config()` accessor |
| `src/daemon/handlers.rs` | Add coordinator.get_goal handler + dispatch entry, add test |
| `src/tui/views/agents.rs` | Show target_id/query for thinking-plane agents |
| `src/integration_tests.rs` | Add 9 e2e tests |

## Implementation Order

1. Add `RecordCreated`/`AgentSpawned` to `ActionResult`, remove `NotYetImplemented`
2. Wire all 8 actions in executor.rs
3. Update `format_action_summary()` in coordinator.rs
4. Add `coordinator.get_goal` handler
5. Add `config()` to bridge, add lock check to WriteFile
6. Fix TUI agents view
7. Add integration tests
8. `otto ci`
