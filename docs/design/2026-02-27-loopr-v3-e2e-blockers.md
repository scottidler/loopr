# Design Document: Loopr v3 — E2E Pipeline Blockers & Correctness Tests

**Author:** Scott Aidler + Claude
**Date:** 2026-02-27
**Status:** Draft
**Review Passes Completed:** 4/5

## Summary

Six integration bugs prevent Loopr's autonomous pipeline from working end-to-end. All 112 design features are implemented, but the system has never successfully completed a full autonomous run (goal → plan → spec → phase → work items → implementer → bundle → review → tick). This document fixes the 6 blockers and adds e2e tests that exercise the actual agent loops with mock LLMs, forcing correctness at the integration boundary.

## Problem Statement

### Background

MVP4 validation (commit `2624642`) wired all Coordinator actions and passed 1069 unit/integration tests. Manual e2e testing (commit `51037b2`) found and fixed 4 issues (parse resilience, FSM gaps, IPC limits). But a real autonomous run — setting a coordinator goal and letting agents build a project — fails within the first few iterations.

### Problem

The autonomous pipeline breaks at 6 points:

1. **Transition action param mismatch** — Every `Transition` action from any agent silently fails because the executor sends `"target"` but handlers expect `"target_status"`
2. **Coordinator assigns agents to Draft work items** — No status validation before spawning implementers
3. **Implementer context too thin** — ~538 tokens, missing project goal, state summary, and system context
4. **No action result feedback** — Only the last action's summary carries to the next iteration; multi-action failures are lost
5. **Coordinator iteration counter stale** — Only updates in stores after the loop exits, not per-cycle
6. **No coordinator auto-restart** — Design says "auto-restart after `idle_interval_secs * 2`" but no code exists

### Goals

- Fix all 6 blockers so the autonomous pipeline can complete
- Add e2e tests exercising actual agent loops (`run_coordinator`, `run_implementer`) with mock LLMs
- Each test targets a specific blocker, proving the fix works at the integration level

### Non-Goals

- Changing the agent architecture or LLM prompts
- Adding new agent types or actions
- Real LLM integration tests (those require API keys and are non-deterministic)

## Proposed Solution

### Overview

Six targeted fixes across 4 files (`executor.rs`, `implementer.rs`, `coordinator.rs`, `context.rs`), plus 6 e2e tests. Ordered by dependency — each fix is independently testable.

### Fix 1: Transition Action Param Name (Critical)

**File:** `src/agents/executor.rs:412`
**Change:** One line — `"target"` → `"target_status"`

```rust
// Before:
let params = serde_json::json!({ "id": id, "target": target_state, "role": effective_role });
// After:
let params = serde_json::json!({ "id": id, "target_status": target_state, "role": effective_role });
```

**Impact:** Unblocks ALL agent transition actions. Without this, coordinators cannot transition plans/specs/phases/work items, and implementers cannot transition bundles. This is the single most critical fix.

### Fix 2: Auto-Transition Work Items on AssignAgent

**File:** `src/agents/executor.rs`, `AssignAgent` handler (~575-606)
**Change:** Before the existing `agent.start` bridge call, add status validation for implementer assignments.

Logic:
1. Read work item via `bridge.request("work_item.get", ...)`
2. If Draft → auto-transition Draft→Ready→InProgress via bridge with coordinator role
3. If Ready → auto-transition Ready→InProgress
4. If InProgress → proceed (already correct)
5. If any other state → return ActionError

~30 lines inserted before existing code. Uses only existing bridge IPC methods.

### Fix 3: Accumulate All Action Results

**File:** `src/agents/implementer.rs`, `run_iteration()` action loop (~208-221)
**Change:** Replace `last_summary = summary` with accumulation.

```rust
// Before:
let mut last_summary = String::new();
for action in &actions {
    // ...
    last_summary = summary;
}
Ok(IterationOutcome::Continue(last_summary))

// After:
let mut summaries = Vec::new();
for action in &actions {
    // ...
    summaries.push(summary);
}
Ok(IterationOutcome::Continue(summaries.join("\n")))
```

~3 lines changed. The `previous_summary` token budget (1000 tokens) accommodates ~10 action summaries comfortably.

### Fix 4: Persist Coordinator Iteration Per-Cycle

**File:** `src/agents/coordinator.rs`, `run_coordinator()` loop body (~500-535)
**Change:** After updating `session.iteration`, write it back to stores immediately.

```rust
session.iteration = iteration;
// Persist iteration to stores so agent list/status reflect progress
{
    let mut sessions = stores.agent_sessions.write().unwrap();
    if let Some(s) = sessions.get_mut(&session.id) {
        s.iteration = session.iteration;
    }
}
```

~5 lines added. Apply the same pattern in `run_implementer()` for consistency.

### Fix 5: Enrich Implementer Context

**Files:** `src/agents/context.rs` (new method), `src/agents/implementer.rs` (call it)

**Change 1 — context.rs:** Add `with_coordinator_goal()` method to `ContextBuilder`. Reads the active goal from `stores.coordinator_goals` and sets a new `coordinator_goal: Option<String>` field. In `build()`, emit a `## Project Goal` section before the hierarchy. This is a natural extension of the existing builder pattern — same as `with_state_summary()` but for the goal.

**Change 2 — context.rs:** In `build()`, after the hierarchy section, add sibling work items from the same phase. The builder already has `self.work_item` and access to `self.stores` — read `stores.work_items` and filter by matching `phase_id`, excluding the current work item. Format as: `## Sibling Work Items\n- [status] title`. This fits in the existing `state_summary` budget.

**Change 3 — implementer.rs:** In `run_iteration()`, add `.with_coordinator_goal()` and `.with_state_summary(build_implementer_summary(stores, work_item_id))` to the builder chain.

`build_implementer_summary()` is a small helper in implementer.rs that builds a focused string: active locks on resources, active agents working on sibling work items.

These fit within the existing `state_summary: 2000` token budget that's currently unused for Implementer role. The goal adds ~50 tokens, sibling WIs ~100 tokens, locks/agents ~100 tokens.

### Fix 6: Coordinator Auto-Restart on Failure

**File:** `src/agents/executor.rs`, `run_agent_task()` (~106)
**Change:** Wrap the coordinator's `run_agent_loop()` call in a retry loop.

```rust
let result = if agent_type == AgentType::Coordinator {
    let max_restarts = 3u32;
    let restart_delay = stores.config.agents.coordinator.idle_interval_secs * 2;
    let mut attempt = 0u32;
    let mut result = run_agent_loop(...).await;

    while result.is_err() && attempt < max_restarts {
        attempt += 1;
        warn!("Coordinator {} failed (attempt {}/{}), restarting in {}s",
              session_id, attempt, max_restarts, restart_delay);
        tokio::time::sleep(Duration::from_secs(restart_delay)).await;
        // Check cancellation during sleep
        let cancelled = stores.agent_sessions.read().unwrap()
            .get(&session_id)
            .map_or(false, |s| s.status == AgentStatus::Cancelled);
        if cancelled { break; }
        result = run_agent_loop(...).await;
    }
    result
} else {
    run_agent_loop(...).await
};
```

~25 lines. Only affects Coordinator. Other agents follow the existing single-attempt path. Max 3 restarts with configurable delay prevents infinite loops.

### E2E Tests

All tests use `MockLlm` (canned JSON responses) and `CapturingLlm` (captures prompts for verification). No real API calls.

| Test | Verifies | Strategy |
|------|----------|----------|
| `test_transition_action_uses_correct_param` | Fix 1 | Execute Transition action, verify it succeeds (not "target_status required") |
| `test_assign_agent_auto_transitions_draft` | Fix 2 | AssignAgent on Draft WI, verify WI is InProgress afterward |
| `test_action_results_accumulate` | Fix 3 | Multi-action iteration, verify joined summary contains all results |
| `test_coordinator_iteration_persists` | Fix 4 | Run 3 coordinator iterations, read stores after each, verify counter |
| `test_implementer_context_includes_goal` | Fix 5 | CapturingLlm verifies prompt contains "Project Goal" and goal text |
| `test_coordinator_assigns_implementer_completes` | All fixes | Full pipeline: coordinator assigns → implementer writes file → verifies file exists |

### Implementation Plan

| Phase | Files | Changes |
|-------|-------|---------|
| 1 | `executor.rs` | Fix 1 (transition param), Fix 2 (auto-transition) |
| 2 | `implementer.rs` | Fix 3 (accumulate results), Fix 4 (iteration persist in implementer) |
| 3 | `coordinator.rs` | Fix 4 (iteration persist in coordinator) |
| 4 | `context.rs`, `implementer.rs` | Fix 5 (enrich context) |
| 5 | `executor.rs` | Fix 6 (auto-restart) |
| 6 | `executor.rs`, `implementer.rs`, `coordinator.rs` | All 6 e2e tests |
| 7 | Verify | `otto ci`, manual autonomous run |

## Alternatives Considered

### Alternative 1: LLM-in-the-Loop E2E Tests
- **Description:** Use real Anthropic API calls in e2e tests
- **Pros:** Tests the actual LLM response parsing path
- **Cons:** Non-deterministic, requires API key, costs money, slow
- **Why not chosen:** Mock LLMs are sufficient to verify the pipeline mechanics. Real LLM testing is done manually.

### Alternative 2: Inject Mock LLMs into Spawned Agent Tasks
- **Description:** Modify `run_agent_task()` to accept a factory for LLM clients
- **Pros:** Could test the full spawn-to-completion path
- **Cons:** Invasive change to production code, adds complexity for test-only benefit
- **Why not chosen:** Testing at the `run_coordinator()`/`run_implementer()` level achieves the same coverage without production code changes.

## Technical Considerations

### Dependencies
- No new crate dependencies
- All fixes use existing infrastructure (bridge IPC, stores, FSM transitions)

### Testing Strategy
- Each fix has a dedicated e2e test targeting that specific blocker
- Final integration test (`test_coordinator_assigns_implementer_completes`) exercises the full pipeline
- All tests use tmpdir fixtures with unique IDs for isolation
- `otto ci` validates no regressions (1071+ existing tests)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Auto-transition side effects (Fix 2) | Low | Med | Follows exact FSM rules; coordinator sees updated state next iteration |
| Lock contention from per-cycle persist (Fix 4) | Low | Low | Sub-microsecond HashMap write behind RwLock |
| Auto-restart infinite loop (Fix 6) | Low | High | Max 3 restarts + configurable delay + cancellation check |
| Context enrichment inflates token usage (Fix 5) | Low | Low | Uses existing unused 2000-token budget; goal adds ~50 tokens |

## Open Questions

- [x] Should auto-transition in AssignAgent be coordinator-only? — Yes, it uses coordinator role for transitions
- [x] Should the Transition param fix be `"target_status"` or should handlers accept both? — Use `"target_status"` to match existing convention

## References

- Design doc: `docs/design/2026-02-26-loopr-v3-mvp4.md` (Coordinator loop, section on auto-restart)
- Validation fixes: `docs/design/2026-02-27-loopr-v3-mvp4-validation.md`
- E2E test findings: commit `51037b2` (4 fixes from manual testing)
