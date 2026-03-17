# Remaining Gaps

Small items surfaced by `2026-02-27-audit-fixes.md` and `2026-02-27-completion-gaps.md` that are not yet addressed. These docs are otherwise Implemented.

## From audit-fixes.md

| # | Gap | Severity | Notes |
|---|-----|----------|-------|
| 16 | Work `depends_on` cycle detection | Low | BFS/DFS check missing. Current code validates deps exist but not acyclicity. |

## From completion-gaps.md

| # | Gap | Severity | Notes |
|---|-----|----------|-------|
| 10 | Tool SIGTERM -> wait 5s -> SIGKILL escalation | Low | SIGTERM sent, but `kill_on_drop` used instead of timed escalation. `child` consumed by `wait_with_output`. |
| 11 | Agent session wall-clock timeouts | Medium | `session_timeout_secs` config exists but `run_agent_task` never wraps with `tokio::time::timeout`. |
| 22 | Bundle `loc_changed` field + `max_loc_changed` enforcement | Low | `max_files_touched` enforced. `loc_changed` field never added to Bundle struct. |

## From semantic-decomposition.md (Layer 7)

| Gap | Severity | Notes |
|-----|----------|-------|
| Upward feedback / bubble-up logic | Medium | `decomposition_attempts` tracking in place. No `ReviseParent` action or bubble-up that transitions parent to Draft. No `max_bubble_up_depth`. |
| Collaborative Plan interview IPC | Medium | `CoordinatorFsmState::Interviewing` exists. `coordinator.interview_respond` and `coordinator.approve_plan` handlers not wired. |
| Coverage gate in Coordinator loop | Medium | Coverage Evaluator module done. Not yet called from Coordinator's decision tree during iteration. |

## From file-touch-broadcasting.md

| Gap | Severity | Notes |
|-----|----------|-------|
| Auto-lock on WriteFile | Medium | No advisory lock auto-acquisition before writes. |
| Lock cleanup on agent exit | Medium | No automatic lock release when agent session ends. |
