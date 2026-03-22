# Design Document: Coverage Bubble-Up and Headless Mode

**Author:** Scott Idler
**Date:** 2026-03-21
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Two features that close critical gaps in Loopr's orchestration pipeline. Coverage bubble-up (P2) prevents the system from grinding on bad Plans by escalating repeated failures upward. Headless mode (P3) enables fire-and-forget execution via `loopr run <goal>`, unlocking overnight runs and CI testing.

## Problem Statement

### Background

Loopr's orchestration spine runs: Plan -> Spec -> Phase -> Work -> Bundle -> Tick -> Done. The Coverage Evaluator exists (domain model, LLM evaluator, executor action, config) but is never invoked by the Coordinator. When decomposition produces bad children (Specs that don't cover the Plan, Works that miss the Phase), the system has no way to detect or recover - it grinds forward on a broken plan.

Separately, Loopr requires the TUI to submit goals and monitor progress. There's no way to fire a goal from the CLI and walk away.

### Problem

**Coverage bubble-up**: The Coordinator decomposes Plans into Specs, Specs into Phases, Phases into Works. If any level's decomposition is incomplete, everything downstream is wasted work. The CoverageEvaluator can detect this, but:
- It's never called during the Planning state
- Incomplete verdicts are never handled
- `decomposition_attempts` counters exist in CoordinatorState but are never incremented
- Parent revision (Draft -> revise) on repeated failure doesn't exist
- `max_decomposition_attempts: 3` and `max_bubble_up_depth: 2` config values exist but are dead code

**Headless mode**: The existing CLI has `coordinator set-goal "..."` which works. But there's no way to block until completion, get exit codes, or run atomically. The Karpathy "run overnight, review in morning" pattern requires `loopr run <goal>` that handles the full lifecycle.

### Goals

- **G1**: Coordinator invokes CoverageEvaluator after decomposing each hierarchy level
- **G2**: Incomplete coverage triggers re-decomposition (up to `max_decomposition_attempts`)
- **G3**: Exhausted attempts bubble up to parent (parent transitions Draft, re-decomposes)
- **G4**: `loopr run <goal>` submits a goal, monitors progress, exits with status code
- **G5**: Headless mode works without TUI - daemon-only operation

### Non-Goals

- Changing the CoverageEvaluator's LLM logic (it works - just need to call it)
- Multi-pass Reviewer (P5 - separate concern)
- Interview refinement (P1 - the e2e run will determine if this matters)
- Real-time progress output in headless mode (status polling is sufficient)

## Proposed Solution

### Part A: Coverage Bubble-Up

#### Overview

After the Coordinator decomposes any parent into children (Plan -> Specs, Spec -> Phases, Phase -> Works), it calls `EvaluateCoverage` before activating children. If Incomplete, it re-decomposes. If attempts exhausted, it revises the parent.

#### Architecture

```
Coordinator Planning State
  |
  ├── Decompose Plan -> Specs
  │     └── EvaluateCoverage(plan, specs)
  │           ├── Complete -> activate Specs, continue
  │           └── Incomplete -> increment attempts
  │                 ├── under max_decomposition_attempts -> re-decompose
  │                 └── at max -> bubble_up(plan)
  │                       ├── under max_bubble_up_depth -> revise Plan, re-interview
  │                       └── at max -> NeedHelp (ask user)
  |
  ├── For each Spec: Decompose -> Phases
  │     └── EvaluateCoverage(spec, phases) [same logic]
  |
  └── For each Phase: Decompose -> Works
        └── EvaluateCoverage(phase, works) [same logic]
```

#### Where It Hooks In

The Coordinator's Planning state in `coordinator.rs` currently generates children via LLM and immediately activates them. The change:

1. **After decomposition, before activation**: Call `EvaluateCoverage` via the existing executor action
2. **On Complete**: Activate children, proceed normally
3. **On Incomplete**: Increment `decomposition_attempts[parent_id]`, re-prompt LLM with gap feedback
4. **On exhausted attempts**: Call `bubble_up(parent_id)`:
   - Transition parent back to Draft
   - Create a Learning with the coverage gaps
   - If parent is Plan: transition Coordinator FSM back to Interview (or NeedHelp if at max depth)
   - If parent is Spec/Phase: re-decompose the grandparent

#### Data Model Changes

None. All required fields already exist:

- `CoordinatorState.decomposition_attempts: HashMap<String, u32>` - exists, unused
- `config.max_decomposition_attempts: u32` (default 3) - exists, unused
- `config.max_bubble_up_depth: u32` (default 2) - exists, unused
- `CoverageReport` with `CoverageVerdict::Complete | Incomplete` - exists, works
- `CoverageGap` with severity (Critical/Minor) - exists, works

#### Implementation Plan (Coverage)

**Phase 1: Wire coverage evaluation into Coordinator Planning**
- After Plan -> Specs decomposition, call `EvaluateCoverage`
- After Spec -> Phases decomposition, call `EvaluateCoverage`
- After Phase -> Works decomposition, call `EvaluateCoverage`
- On Complete: activate children (existing behavior)
- On Incomplete: log gaps, increment counter, re-prompt LLM with gap info

**Phase 2: Wire re-decomposition loop**
- Check `decomposition_attempts[parent_id] < max_decomposition_attempts`
- If under: re-decompose with gap context injected into prompt
- If at max: call `bubble_up()`

**Phase 3: Wire bubble-up**
- `bubble_up(parent_id)`: transition parent to Draft, abandon children, create Learning
- If parent is Plan: FSM -> Interview (ask user for clarification) or NeedHelp
- If parent is Spec/Phase: re-decompose grandparent
- Check `bubble_up_depth < max_bubble_up_depth` to prevent infinite escalation
- At max depth: FSM -> NeedHelp

---

### Part B: Headless Mode

#### Design Philosophy: Plan Quality is the Ceiling

The vault's research converges on one thesis: **specification collapse kills agents**. A vague goal fed to automation produces wasted work downstream - every layer amplifies the original ambiguity.

> "90% of vibe coding failures trace to two causes: moving so fast you never define what you want, and confusing 'works on my laptop' with production-ready." - Agent Debate synthesis

> "Skip requirements conversation" is listed as a **Wrong Approach**. Correct: "Spend 30+ minutes on requirements discussion." - RWL Research

> "Agentic engineering is knowing what will happen in your system so well you don't need to look. Vibe coding is not knowing and not looking." - Stripe

Loopr's TUI chat funnel (Chat -> Interview -> PlanDraft -> Accept) exists specifically to prevent specification collapse. The human-LLM conversation builds shared context, surfaces ambiguity, and sharpens acceptance criteria *before* any automation runs.

**Headless mode is not a replacement for the chat funnel.** It is for two scenarios:

1. **The plan is already written.** Human did the thinking offline, wrote a detailed plan, and wants to hand it to Loopr for execution. This is Stripe's "outloop" pattern - you show up for planning and review, agents handle execution.

2. **The task is trivially well-defined.** "Add a /version slash command" is unambiguous enough that auto-planning can sharpen it without human input. Most real tasks don't qualify.

For anything in between, `loopr run` should reject the goal with a clear error rather than waste API costs grinding on a vague spec.

#### Goal Clarity Gate

Before submitting a goal to the Coordinator, `loopr run` validates goal clarity using a fast LLM call (Sonnet). The gate checks:

- **Specificity**: Does the goal describe a concrete, bounded change? ("Add X to Y" vs "improve the system")
- **Acceptance criteria**: Can you tell when it's done? ("displays version string" vs "works better")
- **Scope**: Is it scoped to a single concern? ("add /version command" vs "refactor the TUI and add features")

If the goal fails the clarity gate:

```
$ loopr run "make things better"
Error: Goal is too vague for autonomous execution.

The goal lacks:
  - Specific deliverable (what artifact or behavior changes?)
  - Acceptance criteria (how do you know it's done?)

Options:
  1. Be more specific: loopr run "Add a /version slash command that displays crate::version()"
  2. Provide a plan:   loopr run --plan docs/my-feature-plan.md
  3. Use the TUI:      loopr (chat with the LLM to refine the goal interactively)
```

If the goal passes, execution proceeds normally.

#### Overview

`loopr run` ensures the daemon is running, validates goal clarity, submits a CoordinatorGoal, polls until terminal state, and exits with a status code.

#### Architecture

```
loopr run "Add /version command"
  |
  ├── ensure_daemon() (start if not running, kill if stale version)
  ├── Goal clarity gate (fast Sonnet call)
  │     ├── Pass -> continue
  │     └── Fail -> print feedback, exit 3
  ├── IPC: coordinator.set_goal(goal) -> goal_id
  ├── IPC: coordinator.accept_plan(plan) [if --plan provided]
  ├── Poll loop:
  │     ├── IPC: coordinator.goal -> check FSM state
  │     ├── Sleep 5s between polls
  │     ├── Print status line on change
  │     └── Break on terminal state or timeout
  └── Exit code:
        ├── 0 = GoalComplete
        ├── 1 = Failed / Abandoned / timeout
        ├── 2 = NeedHelp (human intervention required)
        └── 3 = Goal rejected by clarity gate

loopr run --plan docs/feature.md
  |
  ├── ensure_daemon()
  ├── Read plan file, extract title as goal
  ├── Skip clarity gate (plan is the spec)
  ├── IPC: coordinator.accept_plan(plan_text)
  ├── Poll loop (same as above)
  └── Exit codes (same as above)
```

#### CLI Design

```
loopr run <GOAL>                     # Validate goal clarity, auto-plan, monitor
loopr run --plan <FILE>              # Skip clarity gate, use plan file (goal extracted from title)
loopr run <GOAL> --plan <FILE>       # Explicit goal + plan file
loopr run <GOAL> --timeout 3600      # Custom timeout (default: 1 hour)
loopr run <GOAL> --no-monitor        # Submit and exit immediately
loopr run <GOAL> --skip-clarity-gate # Bypass goal validation (power users)
```

When `--plan` is provided without `<GOAL>`, the goal is extracted from the plan's first line (title). The clarity gate is skipped because the plan itself is the specification.

#### Implementation Plan (Headless)

**Phase 1: Add `run` CLI command**
- Add `Command::Run` variant to `src/cli/mod.rs`
- Parse args: goal (optional when --plan given), timeout, plan file, no-monitor, skip-clarity-gate
- Wire dispatch in `src/cli/dispatch.rs`

**Phase 2: Implement goal clarity gate**
- Fast Sonnet call with structured output (pass/fail + feedback)
- Prompt: "Does this goal describe a specific, bounded, verifiable change? Respond with JSON: {pass: bool, feedback: string}"
- On fail: print feedback with suggestions (be specific, provide plan, use TUI), exit 3
- Skipped when --plan provided or --skip-clarity-gate flag set

**Phase 3: Implement run logic + polling monitor**
- `ensure_daemon()` already exists - reuse it
- Connect IPC client, handshake
- If --plan: read file, extract title, send `coordinator.accept_plan(plan_text)`
- Else: send `coordinator.set_goal(goal)`
- If --no-monitor: print goal_id and exit 0
- Poll loop: check FSM state every 5s, print status changes, exit on terminal state or timeout
- Handle Ctrl+C gracefully (print goal_id for later `loopr coordinator goal` check)

## Alternatives Considered

### Alternative 1: Event stream instead of polling for headless mode
- **Description:** Subscribe to daemon broadcast channel, render events in real-time
- **Pros:** Immediate feedback, no polling delay, richer output
- **Cons:** Requires event loop in CLI, more complex, broadcast channel is in-process only (would need IPC event subscription)
- **Why not chosen:** Polling is simpler, sufficient for v1. Event streaming can be added later. The TUI already handles the rich event display.

### Alternative 2: Coverage check only at Plan level
- **Description:** Only evaluate Plan -> Specs coverage, skip lower levels
- **Pros:** Simpler, fewer LLM calls
- **Cons:** Bad Specs still produce bad Phases and Works. The cascade is the whole problem.
- **Why not chosen:** The evaluator already supports all three boundaries. Checking all levels is the correct behavior per the design doc.

### Alternative 3: Automatic re-interview on bubble-up
- **Description:** When bubble-up reaches Plan level, automatically re-enter Interview mode and ask the user for more detail
- **Pros:** Self-healing without NeedHelp
- **Cons:** In headless mode, there's no user to interview. Even in TUI, automatic re-interview can be confusing.
- **Why not chosen:** Use NeedHelp instead. The user can re-plan via `/plan` in TUI or re-run in headless. Automatic re-interview is a P1 refinement.

### Alternative 4: Accept any goal string without validation
- **Description:** Let `loopr run` accept any goal and let the Coordinator figure it out
- **Pros:** Simpler, no extra LLM call, no gate to maintain
- **Cons:** Vague goals waste API costs grinding through bad plans. The RWL research explicitly warns: "Garbage-in-garbage-out: Poor specs produce poor implementations." The Coordinator will thrash on ambiguous goals and eventually NeedHelp anyway - after burning through decomposition attempts.
- **Why not chosen:** A single fast Sonnet call (~1s, ~$0.001) to gate is far cheaper than 10+ Opus calls grinding on a vague plan. Fail fast, fail cheap.

## Technical Considerations

### Dependencies

**Coverage bubble-up:**
- `CoverageEvaluator` in `src/evaluator/mod.rs` (exists, works)
- `AgentAction::EvaluateCoverage` in executor (exists, works)
- `CoordinatorState.decomposition_attempts` (exists, dead code)
- LLM API calls (one per coverage evaluation - ~3 per decomposition level)

**Headless mode:**
- `ensure_daemon()` (exists)
- `IpcClient` (exists)
- `coordinator.set_goal` handler (exists, works)
- `coordinator.goal` handler (needs verification - may need to add FSM state to response)
- **Goal clarity gate**: Fast Sonnet call (claude-sonnet-4-6) for goal validation. Requires `ANTHROPIC_API_KEY` env var. Skipped when `--plan` or `--skip-clarity-gate` is provided.

### Performance

- Coverage evaluation adds one LLM call per hierarchy level (Plan, Spec, Phase). For a 1-phase, 3-work plan: 3 extra LLM calls. Acceptable.
- Re-decomposition doubles the LLM calls for that level. With max 3 attempts, worst case is 9 extra calls per level. Acceptable for correctness.
- Headless polling at 5s intervals is negligible.

### Testing Strategy

**Coverage bubble-up:**
- Unit test: `increment_decomposition_attempts` / `reset_decomposition_attempts` (state management)
- Unit test: bubble-up decision logic (under max -> retry, at max -> escalate, at depth max -> NeedHelp)
- Integration test: mock evaluator returning Incomplete, verify re-decomposition triggers
- E2E: run with an intentionally vague goal, verify system asks for help instead of grinding

**Headless mode:**
- Unit test: CLI arg parsing for `run` command
- Unit test: exit code mapping (GoalComplete -> 0, Failed -> 1, NeedHelp -> 2)
- Integration test: `loopr run --no-monitor "test goal"` exits cleanly
- E2E: `loopr run "Add /version command"` completes autonomously (after e2e run fixes)

### Rollout Plan

Coverage bubble-up and headless mode are independent - implement in either order. Coverage bubble-up is more important for correctness. Headless mode is more important for workflow.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coverage evaluator LLM gives false Incomplete verdicts | Medium | Medium | Log verdicts + gaps; tune evaluator prompt; Critical gaps only trigger retry (Minor gaps are warnings) |
| Bubble-up creates infinite escalation loop | Low | High | `max_bubble_up_depth` config caps depth; NeedHelp is the escape hatch |
| Re-decomposition produces same bad children | Medium | Medium | Gap context from previous attempt is injected into re-decomposition prompt; after 3 attempts, bubble up |
| Headless mode hangs on daemon crash | Low | Medium | Timeout kills the poll loop; handle IPC disconnect as failure |
| `loopr run` interacts poorly with TUI running simultaneously | Medium | Low | Both submit goals via IPC; daemon handles multiple goals by deactivating old ones. Document: don't run both. |
| Clarity gate rejects valid goals (false negative) | Medium | Low | `--skip-clarity-gate` flag as escape hatch; tune prompt to favor permissiveness for specific goals |
| Clarity gate accepts vague goals (false positive) | Low | Medium | Gate is a cheap first filter, not the only defense; Coordinator's Interview state and Coverage Evaluator provide deeper validation |

## Open Questions

- [ ] Should Critical vs. Minor coverage gaps have different retry behavior? (e.g., retry only on Critical gaps, warn-and-proceed on Minor)
- [ ] Should `loopr run` auto-start the daemon, or require it to be running?
- [ ] What format should the headless progress output use? (human-readable lines, JSON, or silent with exit code only?)
- [ ] Should the clarity gate prompt be configurable (e.g., via `.loopr/clarity-gate.pmt`)?

## References

- Semantic decomposition design (Draft): `docs/design/2026-03-03-semantic-decomposition.md`
- Coverage evaluator: `src/evaluator/mod.rs`, `src/domain/coverage.rs`
- Coordinator state tracking: `src/domain/coordinator_state.rs`
- CLI commands: `src/cli/mod.rs`
- Oracle knowledge extraction: `docs/2026-03-21-oracle-knowledge-extraction-next-steps.md` (P2 + P3)
- Existing `coordinator set-goal` handler: `src/daemon/handlers.rs:3860`
- RWL Research: `ralph-wiggum-loop-2.md` - "Spend 30+ minutes on requirements discussion"
- Stripe Minions: `i-studied-stripe-s-ai-agents-vibe-coding-is-already-dead.md` - Blueprint engine, outloop coding
- Agent Debate: `agent-debate-10x-or-hoax.md` - "Specification collapse kills agents"
