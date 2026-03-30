# Design Document: First Autonomous End-to-End Run

**Author:** Scott Idler + Claude
**Date:** 2026-03-30
**Status:** Draft
**Review Passes Completed:** 5/5
**Supersedes:** `2026-03-21-first-end-to-end-run.md`

## Summary

The entire Chat -> Interview -> Plan -> Execute -> Merge pipeline is implemented. No component is missing. The problem is that nobody has run it end-to-end on a real task. This document defines the test task, the expected execution flow against the actual codebase, anticipated failure modes, the fix-forward strategy, and success criteria for the first autonomous run.

## Problem Statement

### Background

Loopr's orchestration spine is complete:
- **Chat funnel**: `/plan` -> `/draft` -> `/accept` -> `IpcAction::AcceptPlan` -> Plan + CoordinatorGoal creation -> Coordinator auto-starts
- **Coordinator FSM**: `Interviewing -> Planning -> ActivatePhase -> Executing -> PhaseGate -> GoalComplete`
- **Implementer RWL**: picks up Ready Work, iterates with 14 built-in tools (Read, Write, Edit, List, Tree, Glob, Grep, Find, Shell, Slash, Fetch, Search, Todo, Plan), creates Bundles
- **Reviewer**: single-pass LLM review of Bundles (Approve / RequestChanges / Reject)
- **Integrator**: merges Accepted Bundles into Ticks, runs Doc Validator, publishes (both an Agent and daemon handlers)
- **Researcher**: codebase search and file discovery for context gathering
- **Completion sweep**: Coordinator deterministically transitions Integrated -> Done
- **TUI feedback**: `format_orchestration_event()` surfaces events in Chat during Executing state; `build_orchestration_status()` includes live status in system prompt
- **Learning system**: accumulates feedback across iterations (reinforce, contradict, promote, demote)

Every piece exists. The wiring between pieces exists. But the system has never been run end-to-end on a real task.

### Problem

Without an end-to-end run, we don't know:
1. Whether the Coordinator's LLM-generated Plan decomposes into valid Specs/Phases/Works
2. Whether the Implementer can pick up Work items and produce code that passes `otto ci`
3. Whether the Reviewer accepts valid Bundles and rejects bad ones
4. Whether the Integrator can merge Bundles without conflicts and produce valid Ticks
5. Whether the phase gating (`is_phase_complete` in `generation.rs`) and goal completion logic fires correctly
6. What the actual failure modes are vs. the theoretical ones

Building more features on untested foundations is a mistake. One real run surfaces the real bugs.

### Goals

- **G1**: Complete one autonomous run: Chat -> Plan -> decomposed Work -> Implementer produces code -> Bundle merged -> Work Done -> Goal Complete
- **G2**: Document every failure encountered and the fix applied
- **G3**: Establish a repeatable test protocol for future runs
- **G4**: Identify which roadmap items (P2-P6) are actually blocking vs. nice-to-have

### Non-Goals

- Interview funnel refinement - use `/plan` + manual chat to produce a good plan, bypass interview quality for now
- Coverage evaluator bubble-up - test with a plan simple enough to not need regeneration
- Multi-pass Reviewer - single-pass review is fine for validation
- Prompt audit - address after we have a working baseline
- Heavy runner lane architecture - tools run in-process for now

## Proposed Solution

### Overview

Run Loopr on itself. Pick a small, well-defined task in the Loopr codebase, use the TUI chat to plan it, accept the plan, and let the orchestration run to completion. Fix issues as they surface. Document each fix.

### The Test Task

**Task**: "Add a `/version` slash command to the chat view that displays the current Loopr version (`crate::version()`) as a system message."

**Why this task:**
- Single file change (`src/tui/input.rs`, where 9 slash commands already exist)
- Clear acceptance criteria (type `/version`, see version string from `GIT_DESCRIBE`)
- Testable (add a test in `input.rs`)
- Verifiable via `otto ci`
- No external dependencies
- Small enough for one Phase with 1-2 Work items

**The target file** already has this pattern for every command - the Implementer just needs to add another arm to the match at lines 275-373 of `src/tui/input.rs`.

### Agent Configuration

Verified from `src/config.rs`:

| Agent | Model | Max Iterations | Timeout | Notes |
|-------|-------|---------------|---------|-------|
| Coordinator | `claude-opus-4-6` | Unbounded (FSM loop) | None (long-lived) | `max_requeries: 3` per step; `max_validation_attempts: 3` for doc generation |
| Implementer | `claude-sonnet-4-6` | 20 | 30 min | Budget-exhaustion prompt injected on iteration 19 |
| Reviewer | `claude-sonnet-4-6` | 5 | 10 min | Typically exits after 1 iteration (single verdict) |
| Researcher | `claude-sonnet-4-6` | 10 | 10 min | Context gathering |
| Integrator | `claude-sonnet-4-6` | N/A | N/A | Agent + daemon handlers (`integrator.validate`, `integrator.publish`) |
| Chat | configurable | 20 | N/A | User-facing |
| Delegate | `claude-haiku-4-5-20251001` | N/A | N/A | Lightweight sub-tasks |

### Execution Flow

#### Phase 1: Pre-flight

1. **API Keys**: Ensure `ANTHROPIC_API_KEY` is exported.
2. **Clean git state**: `git status` must show a clean working directory so the Integrator has a stable base.
3. **Clean stale worktrees**: Run `loopr worktree cleanup` to remove orphaned worktrees from previous runs (`.worktrees/` may contain leftovers).
4. **Initialize TaskStore** (if first run): `loopr init` to ensure the JSONL + SQLite stores exist.
5. **Start the daemon**: `loopr daemon` in a dedicated terminal. Watch stdout for panics.
6. **Connect the TUI**: `loopr` in a second terminal. Verify Dashboard shows "Connected" and version matches.

#### Phase 2: Chat funnel (human + LLM)

7. Type the task prompt into the Chat view: "Add a /version slash command to the chat view that displays the current Loopr version (crate::version()) as a system message."
8. Type `/plan` - TUI transitions to `FunnelState::Interview`.
9. Answer 1-2 clarifying questions from the LLM about acceptance criteria and scope.
10. Type `/draft` - TUI transitions to `FunnelState::PlanDraft`. LLM generates a structured Plan.
11. Review the plan text in Chat. Ensure it describes a single-phase, 1-2 work-item change.
12. Type `/accept` - TUI sends `IpcAction::AcceptPlan(plan_text)` to the daemon.

#### Phase 3: Orchestration (Coordinator)

The daemon's `handle_coordinator_accept_plan` handler:
13. Creates a Plan record (Draft -> Active).
14. Creates a CoordinatorGoal (deactivates any existing active goals - only one goal can be active at a time).
15. Auto-starts the Coordinator agent.

The Coordinator FSM then:
16. **Planning** state: calls `build_spec_prompt()` -> LLM generates a Spec. Validated by Doc Validator (up to `max_validation_attempts` = 3 retries).
17. Decomposes Spec into Phase(s), each Phase into Work item(s).
18. **ActivatePhase** state: activates the first Phase, transitions its Work items to Ready.
19. **Executing** state: Coordinator optionally spawns a Researcher to scan relevant files before assigning work.
20. Assigns an Implementer agent to the Ready Work item.

#### Phase 4: Implementation (Implementer)

21. Daemon spins up a git worktree via `WorktreeManager` at `.worktrees/<work_id>/` on branch `agent/<work_id>`.
22. Implementer runs up to 20 iterations (RWL loop):
    - Uses `Read` tool on `src/tui/input.rs` (path is relative to the worktree root, not the main repo)
    - Uses `Edit` tool to add the `/version` match arm
    - Uses `Shell` tool to run `cargo check` and `cargo test`
    - Self-corrects on compile/test failures
    - On iteration 19, budget-exhaustion prompt forces `propose_bundle`
23. Implementer calls `propose_bundle` - creates a Bundle (status: Proposed).

#### Phase 5: Review and integration

24. Bundle transitions Proposed -> Triaged (Coordinator triage).
25. Reviewer agent spins up, reads the Bundle diff and Work requirements.
26. Reviewer renders a verdict:
    - **Approve**: Bundle transitions Triaged -> Reviewed. Creates a Learning tagged `review:approve`.
    - **RequestChanges**: Bundle transitions to Rejected. Creates a Learning tagged `review:request_changes`. Coordinator must reassign.
    - **Reject**: Bundle transitions to Rejected. Creates a Learning tagged `review:reject`. Coordinator must reassign.
27. Coordinator transitions Bundle Reviewed -> Accepted.
28. Integrator (agent + daemon handlers):
    - `handle_integrator_validate`: Bundle transitions Accepted -> Integrating. Creates a Tick (Open -> Sealing -> Validating), merges the Bundle's worktree branch, runs validation.
    - `handle_integrator_publish`: Tick transitions Validating -> Published. Bundle transitions Integrating -> Merged. Work transitions InReview -> Integrated.
29. Coordinator's deterministic sweep detects the Integrated Work, transitions it to Done.
30. `is_phase_complete()` (in `generation.rs`) returns true.
31. Coordinator enters **PhaseGate** - evaluates phase completion.
32. With all phases done, Coordinator transitions to **GoalComplete**.

#### Status transition summary (happy path)

For reference, the complete status chains exercised by this run:

- **Plan**: Draft -> Active
- **Spec**: Draft -> Active -> Done
- **Phase**: Draft -> Active -> Done
- **Work**: Draft -> Ready -> InProgress -> InReview -> Integrated -> Done
- **Bundle**: Proposed -> Triaged -> Reviewed -> Accepted -> Integrating -> Merged
- **Tick**: Open -> Sealing -> Validating -> Published
- **Coordinator FSM**: Planning -> ActivatePhase -> Executing -> PhaseGate -> GoalComplete

### Monitoring and Observability

Open a third terminal for diagnostics:

```bash
# Full diagnostic dump (FSM state, token usage, conversation)
loopr diagnose dump

# Agent-specific history (show only failed agents)
loopr diagnose agents --failed

# TaskStore state snapshot
loopr diagnose state

# Session list
loopr diagnose sessions

# Session log (latest session, tail mode)
loopr diagnose log --tail
```

**TUI views to watch:**
- **Chat view**: orchestration event broadcasts ("Created Spec", "Assigned Implementer", etc.)
- **Dashboard/Works view**: Work status progression (Ready -> InProgress -> InReview -> Integrated -> Done)
- **Agents view**: Implementer status (Running, ToolExecution, Paused, Failed)

**Log locations:**
- Session logs: `~/.local/share/loopr/sessions/{session_id}/`
- Latest session shortcut: `~/.local/share/loopr/sessions/latest/` (symlink)

### Expected Failure Modes

Ranked by probability, with precise code locations:

| # | Failure Mode | Location | Symptom | Fix |
|---|-------------|----------|---------|-----|
| 1 | Coordinator LLM produces malformed Spec/Phase/Work decomposition | `generation.rs` prompt builders + Doc Validator | Coordinator stuck in Planning; validation loop exhausts `max_validation_attempts` (3) | Tweak prompts in `generation.rs`; relax or fix validation constraints |
| 2 | Implementer can't find or edit the right file in the worktree | `implementer.rs` tool execution | Implementer edits wrong file or path resolution fails in `.worktrees/` | Check tool path routing; verify worktree creation via `loopr worktree list` |
| 3 | Implementer's code fails `cargo check`/`cargo test` | `implementer.rs` RWL loop | Expected - RWL should self-correct. If iterations exhaust (20), check prompts | Improve system prompt context; ensure worktree has full codebase |
| 4 | Bundle stuck in Triaged or Validating | `integrator.rs` + `handlers.rs` | Tick transitions to Failed; Bundle never reaches Merged | Inspect `tick.validation_log` via `loopr tick list` |
| 5 | Worktree rebase conflict during `WorktreeManager::refresh()` | `worktree/manager.rs` | Silent rebase failure; Implementer works on stale code | Check worktree state before Implementer starts; inspect `.worktrees/` |
| 6 | Reviewer rejects valid Bundle | `reviewer.rs` | Bundle transitions to Rejected; Coordinator must reassign | Tune Reviewer system prompt; check that Work requirements and acceptance criteria are clear |
| 7 | PhaseGate stuck - `is_phase_complete` returns false | `generation.rs:477` (not coordinator.rs) | Coordinator loops in Executing/PhaseGate forever | Debug the completion predicate; check Work status in TaskStore |
| 8 | Coordinator doesn't transition Integrated -> Done | `coordinator.rs` deterministic sweep | Work stays in Integrated; phase never completes | Verify sweep runs before LLM consultation in FSM loop |
| 9 | LLM parse failures (prose instead of JSON) | All agents | Self-correction loop fires (up to `max_requeries`); if exhausted, agent fails | Agents have Lifeguard escalation; if repeated, fix prompt to be more explicit about JSON format |
| 10 | Researcher returns too much or too little context | `researcher.rs` | Implementer gets overwhelmed or underinformed | Tune Researcher search scope and prompts |
| 11 | Implementer uses main repo paths instead of worktree paths | `implementer.rs` system prompt + tool routing | Reads/writes hit the main repo instead of `.worktrees/<work_id>/` | Verify tools resolve paths relative to worktree root; check system prompt path context |
| 12 | Stale TaskStore state from previous aborted runs | TaskStore JSONL/SQLite | Coordinator loads an old active goal and tries to resume stale work | Clean up with `loopr init` or manually deactivate old goals |

### Fix-Forward Protocol

When a failure occurs:
1. Stop the system (`/stop` in TUI or `loopr agent stop <id>`).
2. Diagnose root cause: `loopr diagnose dump`, inspect TaskStore state, read agent logs.
3. Apply the minimal code fix to pass the bottleneck.
4. Run `otto ci` to verify the fix doesn't break anything.
5. Commit: `fix(scope): description - first e2e run`
6. Restart the daemon and resume.

**Rules:**
- Do NOT refactor or improve unrelated code.
- Do NOT get sidetracked by large architectural changes.
- Each fix is one commit, one concern.

### Headless Alternative: `loopr run`

For reproducible testing without TUI interaction:

```bash
loopr run "Add a /version slash command to the chat view that displays the current Loopr version (crate::version()) as a system message." --timeout 600
```

Arguments and flags:
- `<goal>`: positional arg - the task description (required)
- `--timeout`: seconds before abort (default: 3600)
- `--plan`: pre-written plan text (skips interview/drafting - goes straight to Coordinator Planning)
- `--no-monitor`: fire-and-forget (submit goal, exit immediately)

Exit codes: 0=GoalComplete, 1=timeout, 2=NeedHelp.

This is valuable for retry-after-fix cycles where re-driving the TUI chat funnel is overhead. Use TUI for the first attempt (to observe), headless for subsequent retries.

**Important caveat**: Without `--plan`, `loopr run` sends `coordinator.set_goal` which puts the Coordinator in `Interviewing` state - it will ask clarifying questions that have no human to answer. For headless runs, always use `--plan` to provide a pre-written plan and skip the interview.

### State recovery between attempts

When a run fails and you fix the code, you have two options:

1. **Clean restart**: Stop daemon, wipe TaskStore state (back up first), `loopr init`, restart. Cleanest but loses all Learnings.
2. **Resume from checkpoint**: The Coordinator FSM state is persisted to TaskStore and reloaded on daemon restart. If the fix unblocks the stall point, restarting the daemon may resume from where it left off. However, if the FSM is in a corrupt state, a clean restart is safer.

For the first E2E run, prefer clean restarts. State recovery is itself an untested feature.

## Alternatives Considered

### Alternative 1: Write integration tests first
- **Description:** Build an automated test harness that mocks LLM responses and exercises the full pipeline.
- **Pros:** Repeatable, CI-able, no API costs.
- **Cons:** Mocking LLM responses means we're testing our mocks, not the system. The hardest bugs are in LLM interaction. Huge upfront investment for uncertain return.
- **Why not chosen:** A real run with a real LLM is the fastest way to find real bugs. Integration tests are valuable after we know what to test.

### Alternative 2: Bottom-up component testing
- **Description:** Test each agent in isolation (Coordinator alone, Implementer alone, etc.) before combining.
- **Pros:** Isolates failures, easier to debug.
- **Cons:** The bugs we care about are at the boundaries between components. Component tests already exist.
- **Why not chosen:** The components work individually (they have tests). The question is whether they work together.

### Alternative 3: Start with a simpler non-TUI task
- **Description:** Instead of `/version` (which touches TUI code), add a unit test to a domain module - removing TUI compilation from the Implementer's critical path.
- **Pros:** Simpler target, fewer compilation dependencies.
- **Cons:** Doesn't exercise a meaningful feature. The whole point is to prove the system can make a user-visible change.
- **Why not chosen:** The `/version` task is already minimal. If the Implementer can't handle adding a match arm to existing code, we have bigger problems to surface.

### Alternative 4: Start with a larger multi-phase task
- **Description:** Pick a task with 5+ Work items to exercise phase gating and parallel work.
- **Pros:** Tests more of the pipeline.
- **Cons:** More moving parts = harder to diagnose. If phase 1 doesn't work, phases 2-5 don't matter.
- **Why not chosen:** Start small, succeed once, then scale up.

## Technical Considerations

### Dependencies

- **LLM API access**: Coordinator (opus), Implementer/Reviewer/Researcher (sonnet), Delegate (haiku) - all require `ANTHROPIC_API_KEY`
- **Git repo state**: Implementer operates in its own worktree (automatic via `WorktreeManager`); Integrator needs a stable integration branch
- **Daemon running**: must be up and healthy before TUI connects; version must match TUI binary (`ensure_daemon` kills stale daemons with mismatched versions)
- **otto/cargo**: Implementer invokes build/test tools via Shell tool executor

### Testing Strategy

This IS the testing strategy. The document defines a test protocol. Success = one task goes from Chat to GoalComplete autonomously. Future runs reuse this protocol with different tasks.

### Rollout Plan

1. Execute the test run as described (Phase 1-5).
2. Fix issues as they surface (fix-forward protocol).
3. Run a second time with a different task to verify fixes hold.
4. Run headless (`loopr run`) to verify the non-TUI path.
5. Document the protocol in CLAUDE.md for future sessions.
6. Update `docs/next-steps.md` to mark P1 as complete and re-evaluate P2-P6 priorities.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM API rate limits or outages during the run | Low | High | Run during off-peak; have API key with sufficient quota |
| Implementer damages repo state (bad commits) | Low | High | Implementer operates in isolated worktree; never touches main |
| Coordinator enters infinite loop (unbounded FSM iterations) | Medium | Medium | Lifeguard escalation to NeedHelp after repeated failures; `/stop` available |
| Validation loop exhaustion (Doc Validator rejects everything) | Medium | Medium | `max_validation_attempts` cap exists; fix prompts or relax constraints |
| Reviewer rejects valid work, causing retry churn | Medium | Low | Single-pass Reviewer is simple; if too aggressive, tune prompt |
| Worktree cleanup fails, leaving orphaned branches | Low | Low | `loopr worktree cleanup` command exists; `git worktree prune` as fallback |
| First run reveals many bugs | High | Low | Expected. Each fix is progress. The alternative is building on broken foundations. |

## Success Criteria

The First E2E run is officially successful when:

1. The `loopr` process gracefully reaches **GoalComplete** (visible in `loopr diagnose state`).
2. Typing `/version` in the TUI prints the output of `crate::version()` as a system message.
3. Running `otto ci` on the resulting codebase passes 100%.
4. The fix log documents every failure encountered and the minimal fix applied.

## Open Questions

- [x] Is there a way to replay/resume a failed run from mid-point, or must we start from scratch? - **Answered**: see "State recovery between attempts" section. Prefer clean restarts for the first run.
- [ ] What branch should the integration target be? Does the Integrator create one automatically?
- [ ] Should the Coordinator spawn a Researcher before assigning the Implementer, or is this task simple enough to skip?
- [ ] If the Implementer times out at 30 minutes, does the Coordinator detect this and retry, or does it hang?

## References

- Orchestration spine: `docs/design/2026-02-25-orchestration-spine.md`
- Chat-to-orchestration bridge: `docs/design/2026-03-17-chat-to-orchestration-bridge.md`
- Coordinator Integrated->Done fix: `docs/design/2026-03-01-coordinator-integrated-to-done.md`
- Previous version of this doc: `docs/design/2026-03-21-first-end-to-end-run.md`
- Multi-level RWL: `docs/design/2026-02-26-multi-level-rwl.md`
- Native tool use: `docs/design/2026-03-04-native-tool-use.md`
