# Design Document: Headless Testing & Auto-Interview

**Author:** Scott Idler
**Date:** 2026-03-03
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Loopr's Coordinator agent currently stalls in the `Interviewing` FSM state when run without a human at the TUI, because nobody answers the interview questions or approves the proposed Plan. This blocks all automated testing, CI validation, and headless operation. This design adds an auto-interview mode that lets the Coordinator bypass or self-answer the interview phase, a mock LLM test layer for deterministic agent testing, and targeted log noise reduction.

## Problem Statement

### Background

The Coordinator FSM follows this flow: `Interviewing → Planning → ActivatePhase → Executing → PhaseGate → GoalComplete`. The `Interviewing` state requires a human to (1) answer interview questions via `coordinator.interview_response` IPC, then (2) approve the proposed Plan via `coordinator.approve_plan` IPC. Only after plan approval does `plan_approved = true` get set, triggering the transition to `Planning`.

In `bin/test-run.sh`, the script sets a goal via `coordinator set-goal` and walks away. The Coordinator asks interview questions, nobody answers, and the entire system idles forever. Workers never get Work items because no Plan/Spec/Phase/Work hierarchy is ever created.

### Problem

1. **Headless operation is impossible.** The Coordinator cannot progress past `Interviewing` without a human answering questions and approving a Plan.
2. **E2E testing is blocked.** `test-run.sh` cannot validate that the full pipeline (Plan → Spec → Phase → Work → Bundle → Tick) works.
3. **Debug logging is noisy.** Per-chunk SSE logging (`debug!("LLM chunk: {} bytes")`) generates 500-2000 log lines per LLM call, drowning out meaningful state transitions.
4. **No deterministic agent tests.** While `MockLlm` exists, there's no test infrastructure for running the Coordinator through a full headless lifecycle with canned responses.

### Goals

- Coordinator can run headlessly: goal-in, artifacts-out, no human interaction required
- Configurable interview behavior: `interactive` (current default), `auto` (self-answer from goal context), `skip` (jump straight to Planning)
- Per-chunk LLM log lines demoted from `debug` to `trace`
- Headless Coordinator lifecycle test with `MockLlm`

### Non-Goals

- TUI changes (the interactive interview flow remains unchanged)
- Mock HTTP server for full-stack integration tests (future work)
- Recorded replay infrastructure (future work)
- Agent dashboard or real-time monitoring UI

## Proposed Solution

### Overview

1. Add an `InterviewMode` enum to `CoordinatorConfig` with three variants: `Interactive`, `Auto`, `Skip`
2. In `Skip` mode, the Coordinator starts in `Planning` state instead of `Interviewing`, and auto-creates a Plan from the goal text
3. In `Auto` mode, the Coordinator self-answers interview questions by synthesizing answers from the goal text and repo context (README, file tree), then auto-approves the resulting Plan
4. Demote LLM chunk logging from `debug!()` to `trace!()`
5. Add a headless Coordinator lifecycle integration test

### Architecture

#### InterviewMode

```rust
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterviewMode {
    /// Default: Coordinator asks questions, human answers via TUI.
    #[default]
    Interactive,
    /// Coordinator generates questions then self-answers from goal + repo context.
    /// Auto-approves the resulting Plan.
    Auto,
    /// Skip Interviewing entirely. Start in Planning state.
    /// Auto-creates a Plan from the goal text.
    Skip,
}
```

Added to `CoordinatorConfig`:
```rust
pub struct CoordinatorConfig {
    // ... existing fields ...
    #[serde(default)]
    pub interview_mode: InterviewMode,  // default: Interactive
}
```

The `#[serde(default)]` attribute ensures existing config files without `interview_mode` continue to work (defaults to `Interactive`).

YAML config:
```yaml
agents:
  coordinator:
    interview_mode: skip  # or: auto, interactive
```

#### Skip Mode — FSM Bypass

When `interview_mode == Skip`:

1. `CoordinatorState::new()` sets `fsm_state = Planning` and `plan_approved = true` — the Coordinator never enters `Interviewing`
2. The Coordinator's first iteration in `Planning` state uses `build_plan_prompt()` to generate a Plan, then the LLM returns a `CreatePlan` action (existing behavior)
3. The executor creates a Draft Plan via `plan.create` as normal — then detects `interview_mode != Interactive` and immediately calls `coordinator.approve_plan` via the bridge, activating the Plan (Draft → Active)
4. Subsequent iterations generate Specs and Phases. `check_fsm_transition()` sees Active Plan + Active Specs + Phases and transitions to `ActivatePhase` (existing behavior, unchanged)

This is the simplest path. The goal text is the entire context. No interview, no Plan approval gate.

**Note on Plan creation actions:** The Coordinator has two actions that create Plans:
- `CreatePlan` — used in `Planning` state (the normal hierarchy generation flow)
- `ProposePlan` — used in `Interviewing` state (proposes a Plan for human approval)

Both call `plan.create` and produce a Draft Plan. In Skip mode, `CreatePlan` is the one that fires (since we start in `Planning`). In Auto mode, `ProposePlan` fires (since we start in `Interviewing`). The auto-approval logic applies to both.

**Implementation details:**
- `CoordinatorState::new()` accepts `InterviewMode` and conditionally sets initial FSM state and `plan_approved`
- `execute_action()` for both `CreatePlan` and `ProposePlan` checks `interview_mode` from config (available via `ctx.stores.config.agents.coordinator.interview_mode`) — if not `Interactive`, auto-approves the Plan by calling `coordinator.approve_plan` via the bridge after creating it
- No changes to `check_fsm_transition()` — it already handles `Planning` → `ActivatePhase` based on existing hierarchy checks

#### Auto Mode — Self-Interview

When `interview_mode == Auto`:

1. Coordinator enters `Interviewing` state as normal
2. When the LLM returns `InterviewQuestion` actions, instead of sending them to the TUI, the executor generates synthetic answers:
   - Extracts answers from the goal text
   - Reads repo context (README.md, file tree via `ls`) if available
   - Constructs a synthetic `InterviewExchange` and appends it to `interview_context`
3. Coordinator continues iterating in `Interviewing` until it proposes a Plan (`ProposePlan` action)
4. The Plan is auto-approved (same as Skip mode)

**Implementation:** In `execute_action()`, when processing `InterviewQuestion` and `interview_mode == Auto`:
- Instead of emitting questions to the TUI via `coordinator.interview_question`, synthesize an answer
- Call `coordinator.interview_response` via the bridge with the synthetic answer — this reuses the existing handler that appends to `interview_context` and persists to TaskStore
- The config is accessible via `ctx.stores.config.agents.coordinator.interview_mode`
- This keeps the interview context accumulation logic unchanged

The synthetic answer is constructed as:
```
Based on the goal: "{goal_text}"
Repository context: {README contents or "no README found"}
File tree: {top-level file listing}
```

This gives the Coordinator enough context to move to `ProposePlan` on the next iteration.

#### Log Noise Reduction

In `src/agents/llm_client.rs`, line 175:
```rust
// Before:
debug!("LLM chunk: {} bytes", text.len());

// After:
trace!("LLM chunk: {} bytes", text.len());
```

This single-line change eliminates 500-2000 debug lines per LLM call. Users who need per-chunk visibility can use `--log-level trace`.

Additionally, add a structured state transition log line in the Coordinator's FSM transition logic:
```rust
info!("[coordinator] {} -> {} (iteration {})", old_state, new_state, iteration);
```

### Data Model

No new TaskStore collections. Only modifications:

- `CoordinatorState::new()` gains an `interview_mode: InterviewMode` parameter
- `CoordinatorConfig` gains `interview_mode: InterviewMode` field (serde default: `Interactive`)

### API Design

No new IPC methods. Existing methods are reused:

- `coordinator.interview_response` — called internally in Auto mode instead of from TUI
- `coordinator.approve_plan` — called internally in Auto/Skip mode

### Implementation Plan

#### Phase 1: Log Noise Reduction

**Files:**
- `src/agents/llm_client.rs` — change `debug!()` to `trace!()` for chunk logging
- `src/agents/coordinator.rs` — add structured FSM transition log line

**Tests:**
- Existing tests pass (no behavioral change)

#### Phase 2: InterviewMode Config

**Files:**
- `src/config.rs` — add `InterviewMode` enum, add field to `CoordinatorConfig`
- `src/agents/coordinator.rs` — thread `interview_mode` into `CoordinatorState::new()`
- `src/domain/coordinator_state.rs` — `new()` accepts `InterviewMode`, sets initial FSM state

**Tests:**
- Unit test: `CoordinatorState::new()` with `Skip` starts in `Planning`
- Unit test: `CoordinatorState::new()` with `Interactive` starts in `Interviewing`
- Config deserialization test with `interview_mode` field

#### Phase 3: Skip Mode

**Files:**
- `src/agents/executor.rs` — in both `CreatePlan` and `ProposePlan` handlers, detect `interview_mode != Interactive` and auto-approve the Plan via `coordinator.approve_plan` bridge call after creation

**Tests:**
- Integration test: Coordinator with `MockLlm` + Skip mode starts in Planning, creates Plan via `CreatePlan`, auto-activates it
- Verify Plan transitions Draft → Active
- Verify `plan_approved` is set in `CoordinatorState`

#### Phase 4: Auto Mode

**Files:**
- `src/agents/executor.rs` — synthetic answer generation for `InterviewQuestion` in Auto mode
- `src/agents/coordinator.rs` — thread interview_mode through to executor context

**Tests:**
- Integration test: Coordinator with `MockLlm` + Auto mode generates questions, self-answers, proposes Plan, auto-approves
- Verify `interview_context` accumulates synthetic exchanges

#### Phase 5: test-run.sh Update

**Files:**
- `bin/test-run.sh` — set `interview_mode: skip` in generated config
- Add success criteria checks: assert taskstore has Plan, Spec, Phase records after timeout

**Tests:**
- Manual E2E validation with `TIMEOUT=120 bin/test-run.sh`

## Alternatives Considered

### Alternative 1: Rich Goal Documents

- **Description:** Require the goal to contain all interview answers in a structured YAML/JSON format. Coordinator parses the structured goal and skips Interviewing.
- **Pros:** Explicit, no LLM involvement in answer synthesis
- **Cons:** Breaks the simple string goal interface. Forces users to learn a schema. Two different goal formats to maintain.
- **Why not chosen:** Over-engineers the goal interface. The Coordinator already knows how to generate Plans from text — let it.

### Alternative 2: Pre-recorded Interview Fixtures

- **Description:** Bundle Q&A fixture files with the goal. Coordinator replays answers from fixtures.
- **Pros:** Deterministic, testable, reusable
- **Cons:** Brittle — fixtures break when the LLM asks different questions. Requires maintaining fixtures per project type.
- **Why not chosen:** High maintenance burden. Auto mode achieves the same result without fixtures.

### Alternative 3: Separate Headless Agent

- **Description:** Create a `HeadlessCoordinator` that skips the interview FSM entirely with a different state machine.
- **Pros:** Clean separation of concerns
- **Cons:** Duplicates most Coordinator logic. Two code paths to maintain. Divergence risk.
- **Why not chosen:** The existing Coordinator FSM is flexible enough — just parameterize the interview behavior.

### Alternative 4: TUI-less Interview via stdin/stdout

- **Description:** In headless mode, print questions to stdout and read answers from stdin (or a pipe).
- **Pros:** Works with `expect` scripts
- **Cons:** Doesn't work with LLMs (non-deterministic questions). Requires a human or a script that knows the questions in advance.
- **Why not chosen:** Doesn't solve the fundamental problem of LLM non-determinism. Auto mode is more robust.

## Technical Considerations

### Dependencies

No new dependencies. All changes use existing crates.

### Performance

- Skip mode eliminates 1-2 LLM calls (interview questions + Plan proposal) — faster cold start
- Auto mode adds ~1 synthetic answer generation per interview iteration (negligible)
- Trace-level chunk logging eliminates 500-2000 log writes per LLM call at debug level

### Security

- Auto mode reads repo files (README, file tree) for context — same access the Coordinator already has
- No new external API calls or network access

### Testing Strategy

| Layer | What | How |
|-------|------|-----|
| Unit | `InterviewMode` config parsing | Deserialize YAML with each variant |
| Unit | `CoordinatorState::new()` initial state | Assert FSM state matches interview_mode |
| Integration | Skip mode lifecycle | `MockLlm` + Skip → verify Plan/Spec/Phase creation |
| Integration | Auto mode lifecycle | `MockLlm` + Auto → verify interview_context + Plan |
| E2E | `test-run.sh` headless | Real LLM + Skip mode → verify artifacts in `/tmp` |

### Rollout Plan

1. Merge log noise fix (Phase 1) — immediate quality-of-life improvement
2. Merge InterviewMode config + Skip mode (Phases 2-3) — unlocks headless testing
3. Merge Auto mode (Phase 4) — richer headless experience
4. Update test-run.sh (Phase 5) — CI-ready E2E test

## Edge Cases

### Duplicate Plan creation

The `plan.create` handler rejects a second Plan when one already exists. If the Coordinator in Skip mode sends `CreatePlan` and the auto-approval call to `coordinator.approve_plan` fails (e.g., plan_id mismatch), the Plan remains Draft. The Coordinator's next iteration will see a Draft Plan and attempt to re-generate, hitting the existing-plan guard again. **Mitigation:** The auto-approval call uses the plan_id returned by `plan.create`, so it should always succeed. If it fails, the executor returns `ActionResult::ActionError` and the Coordinator retries.

### Auto mode infinite interview loop

If the LLM in Auto mode keeps generating `InterviewQuestion` and never transitions to `ProposePlan`, the Coordinator loops indefinitely in `Interviewing`. **Mitigation:** The Coordinator's existing `max_iterations` config (default: `u32::MAX`) limits total iterations. For Auto mode, the Coordinator's system prompt should include guidance: "You have sufficient context from the goal. After at most 2 interview rounds, propose a Plan." This is a prompt-level fix, not a code change.

### Config backward compatibility

Existing `loopr.yml` files don't have `interview_mode`. **Mitigation:** `#[serde(default)]` on the field ensures deserialization defaults to `Interactive`. No existing behavior changes.

### plan_approved double-set

In Skip mode, `CoordinatorState::new()` sets `plan_approved = true`. The `coordinator.approve_plan` handler also sets it. This is idempotent and harmless — the handler overwrites `true` with `true`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Skip mode produces poor Plans (no interview context) | Medium | Medium | The goal text is the context. For well-specified goals (like test-run.sh's todo app), this is sufficient. For vague goals, Auto mode preserves interview enrichment. |
| Auto mode synthetic answers confuse the LLM | Low | Medium | Synthetic answers are plain text summaries of the goal + repo. The Coordinator already handles free-text answers. |
| Breaking existing Interactive mode | Low | High | Interactive is the default. Skip/Auto are opt-in via config. All existing tests continue to use Interactive. |
| Log level change hides debugging info | Low | Low | Information is still available at `--log-level trace`. The completion summary line (`info!("LLM response complete: N chars")`) remains at info level. |

## Success Criteria

| Criterion | How to Verify |
|-----------|---------------|
| `interview_mode: skip` starts Coordinator in Planning | Unit test: `CoordinatorState::new()` with `Skip` returns `fsm_state == Planning` |
| Skip mode creates and auto-activates a Plan | Integration test with `MockLlm`: Plan exists with `status == Active` after first iteration |
| Auto mode self-answers and proposes Plan | Integration test: `interview_context` has synthetic exchanges, Plan is Draft then Active |
| `test-run.sh` with `interview_mode: skip` produces artifacts | E2E: taskstore has Plan, Spec, Phase, Work records after 120s |
| LLM chunk logging no longer appears at debug level | Run with `--log-level debug`, grep logs for "LLM chunk" — zero matches |
| Existing Interactive mode unchanged | All existing unit and integration tests pass without modification |

## Open Questions

- [x] Should Auto mode use the LLM to generate synthetic answers, or is a template-based approach (goal text + README) sufficient? **Decision: Template-based.** No extra LLM call needed. The goal text + README is sufficient context for the Coordinator to move to ProposePlan.
- [x] Should Skip mode auto-activate the Plan immediately, or leave it as Draft for the Planning state to pick up? **Decision: Auto-activate immediately** via `coordinator.approve_plan` bridge call in the executor. Leaving it as Draft would require the Planning state to know about Skip mode — cleaner to handle it at the action execution layer.
- [ ] Should `test-run.sh` assert specific success criteria (e.g., "at least 1 Plan, 1 Spec, 1 Phase created") or just check for non-empty taskstore?

## References

- `src/agents/coordinator.rs` — Coordinator FSM, `build_fsm_footer()`, `check_fsm_transition()`
- `src/domain/coordinator_state.rs` — `CoordinatorState`, `CoordinatorFsmState`, `InterviewExchange`
- `src/agents/llm_client.rs:175` — per-chunk debug logging (noise source)
- `src/agents/executor.rs` — `InterviewQuestion` and `ProposePlan` action handling
- `src/config.rs` — `CoordinatorConfig`
- `bin/test-run.sh` — headless E2E test script
- `docs/design/2026-02-26-multi-level-rwl.md` — Coordinator FSM design
