# Design Document: coordinator.accept_plan with Text-Based Submission

**Author:** Scott Idler
**Date:** 2026-03-04
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Extend `coordinator.accept_plan` to accept raw plan text (not just `plan_id`), creating the Plan record in the handler before feeding it into the existing activation logic. Add a `loopr coordinator accept-plan` CLI subcommand so automation scripts like `bin/test-run.sh` can submit a fully formed plan without going through the TUI funnel.

## Problem Statement

### Background

The TUI Chat→Plan funnel now produces raw plan text from a TUI-side LLM conversation. On `/accept` or `Ctrl+a`, the TUI sends `coordinator.accept_plan` with `{ "plan": "<text>" }`. The daemon handler currently only accepts `{ "plan_id": "<id>" }` — it looks up an existing Plan record and activates it.

The executor's internal auto-approve path works because it creates a Plan record first via `plan.create`, then calls `coordinator.accept_plan` with the resulting ID. The TUI doesn't create a Plan record — it just has raw text.

Additionally, `bin/test-run.sh` currently uses `interview_mode: skip` to bypass the interview entirely. There's no way to submit a pre-written plan from the CLI for automation or testing.

### Problem

1. **TUI sends plan text, handler expects plan_id** — the TUI funnel path is broken at the daemon boundary
2. **No CLI entry point for plan submission** — automation must use `interview_mode: skip` and let the Coordinator generate its own plan, which removes control over what gets executed
3. **Naming inconsistency** — the method was `coordinator.approve_plan` (implying a prior proposal); now renamed to `coordinator.accept_plan` to match the TUI's `/accept` command

### Goals

- `coordinator.accept_plan` accepts either `plan_id` (existing record) or `plan` (raw text)
- When `plan` text is provided: create a Plan record, get its ID, then run the same activation logic
- Add `loopr coordinator accept-plan` CLI subcommand accepting plan text from an argument
- `bin/test-run.sh` can submit a plan directly: `loopr coordinator accept-plan "Build a todo app..."`
- Single code path for plan activation regardless of entry point (TUI, executor, CLI)

### Non-Goals

- Changing the executor's existing `plan_id`-based flow (it already works)
- Structured plan parsing (title/description/criteria extraction from text) — the text goes into `description`, title is derived from first line
- Changing the CoordinatorState FSM or `plan_approved` field semantics
- Modifying `bin/test-run.sh` (that's a follow-up once this ships)
- File or stdin input for the CLI subcommand (argument-only for MVP; can add later)

## Proposed Solution

### Handler Changes

Update `handle_coordinator_accept_plan` to resolve a `plan_id` from one of two param shapes:

```
1. If `plan_id` is provided → use it directly (existing behavior)
2. Else if `plan` text is provided → create Plan record, use its ID
3. Else → error
```

When `plan` text is provided:
1. Trim the text; reject if empty
2. Extract title: first non-empty line, truncated to 120 chars. Fallback: `"Accepted Plan"`
3. Create `Plan::new(title, plan_text, "")` — full text goes into `description`, acceptance_criteria left empty
4. Persist to TaskStore (if available) + in-memory HashMap
5. Broadcast `record_created` event (same as `plan.create` handler)
6. Use the new plan's ID for the existing activation logic

After resolving the plan_id (from either path), the existing logic runs unchanged:
- Activate the Plan (Draft → Active)
- Update CoordinatorState: `plan_approved = true`, FSM → Planning
- Broadcast `coordinator.plan_accepted` event
- Return `{ "accepted": true, "plan_id": "<id>" }`

### CLI Subcommand

Add `AcceptPlan` variant to `CoordinatorCmd`:

```
loopr coordinator accept-plan "Title: My Plan\nGoal: Build auth..."
```

Maps to IPC: `coordinator.accept_plan` with `{ "plan": "<text>" }`.

### What Changes

| Component | Before | After |
|-----------|--------|-------|
| `handle_coordinator_accept_plan` | Requires `plan_id` | Accepts `plan_id` OR `plan` text |
| `CoordinatorCmd` enum | `Set`, `Clear`, `Status` | + `AcceptPlan { plan: String }` |
| `coordinator_to_ipc` dispatch | 3 variants | + accept-plan mapping |
| `IpcAction::AcceptPlan` (TUI) | Sends `{ "plan": text }` | No change (already correct) |
| Response JSON | `{ "approved": true, ... }` | `{ "accepted": true, ... }` |

### Edge Cases

| Scenario | Behavior |
|----------|----------|
| `plan_id` provided | Existing behavior: look up and activate |
| `plan` text provided | Create record, then activate |
| Both `plan_id` and `plan` provided | `plan_id` wins (existing record takes priority) |
| Neither provided | Error: `"plan_id or plan text is required"` |
| `plan` text is empty/whitespace | Error: `"plan text is empty"` |
| Draft Plan already exists | Create still succeeds. The `plan.create` handler has a draft-exists guard, but `accept_plan` creates directly via `Plan::new`, bypassing it. Intentional: accepting a plan is a commitment action, not exploratory. |
| No active CoordinatorState | Plan is created and activated, but no FSM transition occurs. The handler silently skips the CoordinatorState update. Same as current behavior. |
| Plan text has no newlines | Title = full text (truncated to 120 chars), description = full text |

## Implementation Plan

### Phase 1: Update handler to accept plan text

1. Refactor `handle_coordinator_accept_plan` to resolve plan_id from either `plan_id` or `plan` param
2. When `plan` is provided: create Plan record, persist to TaskStore + HashMap, broadcast event
3. Update error message when neither param is present
4. Update response to use `"accepted"` instead of `"approved"`
5. Add handler tests for text-based path, backward compat, and error cases

### Phase 2: Add CLI subcommand

1. Add `AcceptPlan` variant to `CoordinatorCmd` in `cli/mod.rs`
2. Add `coordinator_to_ipc` mapping in `cli/dispatch.rs`
3. Add CLI parsing tests
4. Add dispatch mapping tests

## Testing Strategy

- Unit test: handler accepts `plan` text, creates Plan record, activates it, sets `plan_approved`
- Unit test: handler still accepts `plan_id` (backward compat with executor path)
- Unit test: handler rejects when neither param provided
- Unit test: handler rejects empty/whitespace plan text
- Unit test: title extraction from first line of plan text
- Unit test: CLI parses `coordinator accept-plan "text"`
- Unit test: CLI dispatch maps to `coordinator.accept_plan` with `{ "plan": text }`

## Alternatives Considered

### Alternative 1: Two separate IPC methods

- **Description:** Keep `coordinator.accept_plan` for `plan_id` only, add `coordinator.submit_plan` for text
- **Pros:** Clear separation of concerns
- **Cons:** Two code paths for plan activation, naming diverges from TUI `/accept`
- **Why not chosen:** Same outcome, more surface area. Single method with param dispatch is simpler.

### Alternative 2: TUI creates Plan record before sending IPC

- **Description:** TUI calls `plan.create` first, then `coordinator.accept_plan` with the ID
- **Pros:** Handler stays unchanged
- **Cons:** TUI needs two IPC round-trips, must handle partial failure (Plan created but not activated), duplicates executor logic
- **Why not chosen:** Pushes complexity to every caller instead of centralizing in the handler

### Alternative 3: Separate CLI binary or script

- **Description:** A shell script that calls `loopr plan create` then `loopr plan transition ... active`
- **Pros:** No code changes
- **Cons:** Doesn't set `plan_approved` or transition CoordinatorState FSM — plan activation requires more than just a status change. Fragile multi-step process.
- **Why not chosen:** Bypasses the coordinator FSM entirely

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Title extraction from text is naive | Low | Low | Fallback to "Accepted Plan"; user can always provide structured text |
| Draft-exists guard bypassed | Low | Low | Intentional — accept_plan is commitment; guard exists on `plan.create` for exploratory path |
| No consumers of `coordinator.plan_accepted` event | N/A | N/A | Verified: no code reads this event. Safe to ship. |

## Decisions

| # | Question | Decision |
|---|----------|----------|
| 1 | Param precedence | `plan_id` wins if both provided |
| 2 | Title extraction | First non-empty line, truncated to 120 chars, fallback "Accepted Plan" |
| 3 | Draft-exists guard | Bypassed intentionally for accept_plan |
| 4 | Response field name | `"accepted"` (not `"approved"`) — no consumers of old field |
| 5 | CLI input method | Argument only for MVP. File/stdin deferred. |

## References

- [TUI Chat→Plan Funnel](2026-03-04-tui-chat-plan-funnel.md) — the design that created this need
- `src/daemon/handlers.rs` — `handle_coordinator_accept_plan` (line ~4051)
- `src/cli/mod.rs` — `CoordinatorCmd` enum (line ~278)
- `src/cli/dispatch.rs` — `coordinator_to_ipc` (line ~322)
- `src/agents/executor.rs` — executor's CreatePlan + auto-approve flow (line ~624)
- `bin/test-run.sh` — automation use case
