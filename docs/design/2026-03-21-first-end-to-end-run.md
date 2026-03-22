# Design Document: First End-to-End Run

**Author:** Scott Idler
**Date:** 2026-03-21
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

The entire Chat -> Interview -> Plan -> Execute -> Merge pipeline is implemented. No component is missing. The problem is that nobody has run it end-to-end on a real task. This doc defines the test protocol, expected failure modes, and fix-forward strategy to get the first successful autonomous run.

## Problem Statement

### Background

Loopr's orchestration spine is complete:
- **Chat bridge**: `/plan` -> `/draft` -> `/accept` -> `coordinator.accept_plan` IPC -> Plan + CoordinatorGoal creation -> Coordinator agent starts
- **Coordinator FSM**: Interviewing -> Planning -> ActivatePhase -> Executing -> PhaseGate -> GoalComplete
- **Implementer RWL**: Picks up Ready Work, iterates with tools, creates Bundles
- **Reviewer**: Single-pass LLM review of Bundles
- **Integrator**: Merges Accepted Bundles into Ticks, runs Doc Validator, publishes
- **Completion sweep**: Coordinator deterministically transitions Integrated -> Done
- **TUI feedback**: `format_orchestration_event()` surfaces events in Chat during Executing state; `build_orchestration_status()` includes live status in system prompt

Every piece exists. The wiring between pieces exists. But the system has never been run end-to-end on a real task.

### Problem

Without an end-to-end run, we don't know:
1. Whether the Coordinator's LLM-generated Plan decomposes into valid Specs/Phases/Works
2. Whether the Implementer can pick up Work items and produce code that passes `otto ci`
3. Whether the Integrator can merge Bundles without conflicts
4. Whether the phase gating and goal completion logic fires correctly
5. What the actual failure modes are (vs. the theoretical ones)

Building more features (P1-P6) on untested foundations is a mistake. One real run surfaces the real bugs.

### Goals

- **G1**: Complete one autonomous run: Chat -> Plan -> decomposed Work -> Implementer produces code -> Bundle merged -> Work Done -> Goal Complete
- **G2**: Document every failure encountered and the fix applied
- **G3**: Establish a repeatable test protocol for future runs
- **G4**: Identify which P1-P6 items are actually blocking vs. nice-to-have

### Non-Goals

- Interview funnel refinement (P1) - use `/plan` + manual chat to produce a good plan, bypass interview quality for now
- Coverage evaluator bubble-up (P2) - test with a plan simple enough to not need regeneration
- Headless mode (P3) - run via TUI, observe directly
- Multi-pass Reviewer (P5) - single-pass review is fine for validation
- Prompt audit (P6) - address after we have a working baseline

## Proposed Solution

### Overview

Run Loopr on itself. Pick a small, well-defined task in the Loopr codebase, use the TUI chat to plan it, accept the plan, and let the orchestration run to completion. Fix issues as they surface. Document each fix.

### The Test Task

The task must be:
- **Small enough** to complete in one phase with 1-3 Work items
- **Well-defined enough** that acceptance criteria are binary
- **In the Loopr codebase** so the Implementer has access to the repo
- **Verifiable** via `otto ci`

**Candidate task**: "Add a `/version` slash command to the chat view that displays the current Loopr version (`crate::version()`) as a system message."

This is ideal because:
- Single file change (`src/tui/input.rs`)
- Clear acceptance criteria (type `/version`, see version string)
- Testable (add a test in input.rs)
- No external dependencies
- Small enough to not overwhelm the first run

### Test Protocol

**Phase 1: Manual plan creation via chat**

1. Start `loopr` (TUI connects to daemon)
2. Type: "Add a /version slash command to the chat view that displays the Loopr version"
3. Type `/plan` to enter Interview mode
4. Answer the LLM's clarifying questions (acceptance criteria, scope)
5. Type `/draft` to generate the plan
6. Review the plan text in Chat
7. Type `/accept` to hand off to orchestration

**Phase 2: Observe autonomous execution**

8. Watch Chat view for orchestration events (Created Spec, Created Phase, Created Work, etc.)
9. Switch to Dashboard/Works/Agents views to monitor progress
10. Note any errors, stuck states, or unexpected behavior

**Phase 3: Diagnose and fix**

11. If the Coordinator stalls: check `loopr diagnose dump` for FSM state, LLM conversation
12. If the Implementer fails: check iteration logs in `.loopr/runs/`
13. If the Integrator fails: check git state, merge conflicts
14. Fix the issue, restart if needed, continue

**Phase 4: Validate completion**

15. Verify Work items reached Done status
16. Verify the code change was merged to the integration branch
17. Run `otto ci` to confirm the change works
18. Check that Goal shows GoalComplete

### Expected Failure Modes

Based on code review, likely failure points ranked by probability:

| # | Failure Mode | Where | Likely Fix |
|---|-------------|-------|-----------|
| 1 | Coordinator LLM produces malformed Plan decomposition | coordinator.rs Planning state | Fix prompt, add validation/retry |
| 2 | Implementer can't find/edit the right file | implementer.rs tool execution | Check tool configuration, sandbox settings |
| 3 | Implementer's code change fails `otto ci` | implementer.rs validation loop | This is expected - RWL should self-correct. If it exhausts iterations, check max_iterations config |
| 4 | Bundle branch conflicts with integration branch | integrator.rs merge flow | Check base_tick_id, branch naming |
| 5 | Coordinator doesn't transition Integrated -> Done | coordinator.rs deterministic sweep | Verify the sweep runs before LLM consultation |
| 6 | Phase gate doesn't fire (stuck in Executing) | coordinator.rs PhaseGate | Check is_phase_complete logic |
| 7 | LLM parse failures (prose instead of JSON) | coordinator.rs, implementer.rs | Self-correction loop should handle; if not, fix prompt |

### Implementation Plan

**Phase 1: Pre-flight checks**
- Verify daemon starts cleanly: `loopr daemon`
- Verify TUI connects: `loopr` (separate terminal)
- Verify chat works: send a message, get LLM response
- Verify IPC: check Dashboard shows Connected

**Phase 2: Execute the test run**
- Follow the test protocol above
- Keep a log of every event observed and every error encountered
- Screenshot key moments for the design doc

**Phase 3: Fix-forward**
- For each failure: diagnose root cause, apply minimal fix, `otto ci`, commit
- Commit messages: `fix(scope): description - first e2e run`
- Do NOT refactor or improve unrelated code

**Phase 4: Retrospective**
- Update this design doc with actual results
- Categorize fixes: which were real bugs vs. configuration issues vs. prompt tuning
- Re-evaluate P1-P6 priorities based on what we learned

## Alternatives Considered

### Alternative 1: Write integration tests first
- **Description:** Build an automated test harness that mocks LLM responses and exercises the full pipeline
- **Pros:** Repeatable, CI-able, no API costs
- **Cons:** Mocking LLM responses means we're testing our mocks, not the system. The hardest bugs are in LLM interaction. Huge upfront investment for uncertain return.
- **Why not chosen:** A real run with a real LLM is the fastest way to find real bugs. Integration tests are valuable after we know what to test.

### Alternative 2: Bottom-up component testing
- **Description:** Test each agent in isolation (Coordinator alone, Implementer alone, etc.) before combining
- **Pros:** Isolates failures, easier to debug
- **Cons:** The bugs we care about are at the boundaries between components. Component tests already exist.
- **Why not chosen:** The components work individually (they have tests). The question is whether they work together.

### Alternative 3: Start with a larger task to exercise more of the system
- **Description:** Pick a multi-phase task with 5+ Work items
- **Pros:** Tests more of the pipeline (phase gating, parallel work, dependency ordering)
- **Cons:** More moving parts = harder to diagnose failures. If phase 1 doesn't work, phases 2-5 don't matter.
- **Why not chosen:** Start small, succeed once, then scale up. A single-phase task exercises the entire pipeline without the complexity of multi-phase coordination.

## Technical Considerations

### Dependencies

- **LLM API access** - Coordinator, Implementer, Reviewer, and Chat all call the LLM (all default to `claude-sonnet-4-6`, require `ANTHROPIC_API_KEY` env var)
- **Git repo state** - Implementer operates in its own worktree (automatic via WorktreeManager); Integrator needs a stable integration branch
- **Daemon running** - must be up and healthy before TUI connects; version must match TUI binary (`ensure_daemon` kills stale daemons)
- **otto/cargo** - Implementer will invoke build/test tools via tool executor

### Testing Strategy

This IS the testing strategy. The design doc defines a test protocol. Success = one task goes from Chat to Done autonomously. Future runs can reuse this protocol with different tasks.

### Rollout Plan

1. Execute the test run as described
2. Fix issues, commit fixes
3. Run a second time with a different task to verify fixes hold
4. Document the protocol in CLAUDE.md for future sessions

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM API rate limits or outages during the run | Low | High | Run during off-peak; have API key with sufficient quota |
| Implementer damages repo state (bad commits, force pushes) | Low | High | Implementer already operates in isolated worktree via WorktreeManager; run on a branch |
| Coordinator enters infinite loop (LLM keeps retrying) | Medium | Medium | max_iterations config exists; `/stop` command available |
| First run reveals so many bugs it's demoralizing | Medium | Low | Expected. Each fix is progress. The alternative is building features on a broken foundation. |
| Fix for one failure causes regression in another | Low | Medium | `otto ci` after every fix; existing 2141 tests catch regressions |

## Known Configuration

Verified from `src/config.rs`:

| Agent | Model | Max Iterations | Timeout |
|-------|-------|---------------|---------|
| Implementer | claude-sonnet-4-6 | 20 | 30 min |
| Reviewer | claude-sonnet-4-6 | 5 | 10 min |
| Researcher | claude-sonnet-4-6 | 10 | 10 min |
| Chat | (configurable) | 20 | N/A |

- Implementer **already uses worktrees** (`worktree_path` field, managed by `WorktreeManager`)
- Coordinator default iterations: governed by `max_requeries: 3` for validation cap
- All agents default to `claude-sonnet-4-6`

## Open Questions

- [ ] Is there a way to replay/resume a failed run from mid-point, or must we start from scratch?
- [ ] Does the daemon need any config file (`.loopr/config.toml` or similar) to run, or do defaults suffice?
- [ ] What branch should the integration target be? Does the Integrator create one automatically?

## References

- Orchestration spine: `docs/design/2026-02-25-orchestration-spine.md`
- Chat-to-orchestration bridge: `docs/design/2026-03-17-chat-to-orchestration-bridge.md`
- Coordinator Integrated->Done fix: `docs/design/2026-03-01-coordinator-integrated-to-done.md`
- Oracle knowledge extraction (P0-P6 priorities): `docs/2026-03-21-oracle-knowledge-extraction-next-steps.md`
- Existing run infrastructure: `.loopr/runs/` directory for iteration logs
