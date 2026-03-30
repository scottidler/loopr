# Design Document: Revert Coordinator Interviewing FSM Investments

**Author:** Scott Idler + Claude
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 3/3

## Summary

Revert three commits that invested in making the Coordinator's `Interviewing` FSM state work better - a path that the chat funnel design doc (`2026-03-04-tui-chat-plan-funnel.md`) explicitly replaced with TUI-side LLM interviewing. The reusable agent failure propagation infrastructure is preserved.

## Problem Statement

### Background

Loopr has two interview paths:

1. **Chat funnel path** (designed, primary) - TUI-side LLM interviews the user via `chat.submit`. Plan is drafted via `/draft`, accepted via `/accept`, and handed to the daemon via `coordinator.accept_plan`. The Coordinator starts in Planning, never enters Interviewing.

2. **Coordinator FSM path** (legacy/headless) - The Coordinator agent runs in `Interviewing` state, asks questions via IPC, and eventually proposes a Plan. Used by `InterviewMode::Auto` and `InterviewMode::Interactive`.

The chat funnel design doc explicitly called the daemon-side interview "over-engineered" and removed it from the primary user flow. However, the `2026-03-29-conversational-funnel-testing.md` design doc built a persona test harness that tests path 2, and subsequent commits invested in fixing bugs in that path.

### Problem

Three commits (today) invested in improving the Coordinator's `Interviewing` FSM state:

| Commit | Description | Why revert |
|--------|-------------|------------|
| `416f903` | Active interval for Interviewing FSM | Optimizes a path users don't take |
| `1cf5701` | Inject interview history into Coordinator prompt | Fixes a bug in a path users don't take |
| `128ad96` | Fail fast on agent failure in run_persona | Good pattern, but 100% embedded in Coordinator interview test driver |

The agent failure propagation infrastructure these built on is kept:
- `650eadb` - error field on StatusChange event (general protocol improvement)
- `d832f46` - executor emits error details (general improvement)

### Goals

- Revert the three wrong-direction commits
- Preserve agent failure propagation infrastructure
- Leave a clear record of why these were reverted

### Non-Goals

- Rewriting the funnel tests for the chat path (separate effort)
- Removing the Coordinator's Interviewing FSM state entirely (it serves headless/Auto mode)
- Reverting the persona test harness itself (yesterday's commits; separate decision)

## Proposed Solution

### Revert Strategy

Revert in reverse chronological order to avoid conflicts:

1. `git revert 416f903` - Interviewing interval fix (HEAD)
2. `git revert 1cf5701` - interview history injection (HEAD~1 after step 1)
3. `git revert 128ad96` - fail fast in run_persona (isolated to tests/funnel.rs)

Each revert creates a new commit with a clear message explaining the rationale.

### What's Preserved

- `650eadb` - `error` field on `StatusChange` event + `agent_status_failed` constructor
- `d832f46` - executor emits error details in agent failure events
- `c7053c3` - docs: mark agent failure propagation design doc as implemented
- `a6f6476` - version bump to v0.1.16

### What's Lost (intentionally)

- `pending_questions` field on `CoordinatorState`
- `format_interview_context()` method
- Interview history prepended in `build_fsm_footer` Interviewing arm
- `Interviewing` added to active-interval match arm
- Test config overrides for interview intervals
- Fail-fast agent failure detection in `run_persona`
- Serde alias `question` -> `questions` on `InterviewQuestion`
- Design docs: `2026-03-30-interview-context-injection.md`, `2026-03-30-interviewing-interval-fix.md`

The fail-fast pattern is trivial to reimplement when funnel tests are rewritten for the chat path. The interview history and interval fixes address real bugs in the Coordinator's Interviewing state, but that state is not on the critical path for the primary user flow.

## Alternatives Considered

### Alternative 1: Keep the fixes, redirect testing effort

- **Description:** Keep all three commits since they fix real bugs, just stop investing further.
- **Pros:** No revert churn. The fixes are correct for the Interviewing FSM path.
- **Cons:** Leaves code that optimizes an off-ramp path. Creates confusion about which path is primary. Future developers may continue investing in the wrong path.
- **Why not chosen:** Clean revert sends a clear signal about architectural direction.

### Alternative 2: Tease out fail-fast from run_persona

- **Description:** Extract the `agent.status_changed` matching pattern into a reusable helper, revert only the funnel.rs-specific parts.
- **Pros:** Preserves the fail-fast pattern for future test drivers.
- **Cons:** The entire diff is 22 lines in `tests/funnel.rs`. There's nothing to extract - it's all test driver code. The pattern is trivial to reimplement.
- **Why not chosen:** Over-engineering a 22-line revert.

## References

- `docs/design/2026-03-04-tui-chat-plan-funnel.md` - Chat funnel design (the designed path)
- `docs/design/2026-03-03-headless-testing-auto-interview.md` - Headless/Auto interview modes
- `docs/design/2026-03-29-conversational-funnel-testing.md` - Persona test harness design
- `docs/design/2026-03-30-agent-failure-propagation.md` - Agent failure propagation (kept)
