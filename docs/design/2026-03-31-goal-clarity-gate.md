# Design Document: Goal Clarity Gate

**Author:** Scott A. Idler
**Date:** 2026-03-31
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Add a fast, cheap LLM pre-validation step to `loopr run` that rejects vague goals before they enter the autonomous pipeline. A single Sonnet call (~1s, ~$0.001) checks specificity, acceptance criteria, and scope - failing fast instead of burning 10+ Opus calls grinding through decomposition on a goal that will inevitably NeedHelp.

## Problem Statement

### Background

`loopr run <goal>` submits a goal to the Coordinator for autonomous execution. The Coordinator decomposes it into Plan -> Spec -> Phase -> Work, with each level requiring expensive Opus LLM calls. When the goal is vague ("make things better", "improve the system"), every decomposition level amplifies the original ambiguity. The Coverage Evaluator catches this eventually, but only after burning through `max_decomposition_attempts * levels` worth of API calls before bubbling up to NeedHelp.

The TUI chat funnel prevents this through human-LLM conversation that sharpens the goal interactively. But `loopr run` has no human in the loop - it needs a programmatic equivalent.

### Problem

There is no validation between `loopr run <goal>` and `coordinator.set_goal`. Any string - including empty platitudes, multi-concern wishlists, and unmeasurable aspirations - enters the autonomous pipeline at full cost.

### Goals

- **G1**: Reject goals that lack specificity, acceptance criteria, or bounded scope before any Coordinator work begins
- **G2**: Provide actionable feedback explaining why the goal was rejected and how to fix it
- **G3**: Cost less than 1% of the pipeline it guards (one Sonnet call vs dozens of Opus calls)
- **G4**: Be bypassable for power users and plan-based workflows

### Non-Goals

- Replacing the TUI interview process (the gate is a fast filter, not a conversation)
- Guaranteeing that passing goals will succeed (the gate catches obvious problems, not subtle ones)
- Validating plan content (that's the Coverage Evaluator's job)
- Running in the daemon (this is a CLI-side pre-check)

## Proposed Solution

### Overview

Before submitting a goal to the daemon, `loopr run` makes a single non-streaming Anthropic API call using Sonnet to evaluate goal clarity. The response is structured JSON with per-dimension scores and actionable feedback. The CLI computes pass/fail from the scores (all dimensions >= threshold). On failure, the CLI prints the feedback with concrete suggestions and exits with code 3.

### Architecture

```
loopr run "Add /version command"
  |
  +--> ensure_daemon()
  |
  +--> Goal Clarity Gate (CLI-side, async reqwest)
  |      |
  |      +--> Build evaluation prompt with goal text
  |      +--> Single Sonnet API call (non-streaming, max_tokens: 512)
  |      +--> Parse structured JSON response
  |      |
  |      +--> Pass --> continue to coordinator.set_goal
  |      +--> Fail --> print feedback, exit 3
  |
  +--> IPC: coordinator.set_goal(goal)
  +--> [rest of existing run_headless flow]
```

The gate runs after `ensure_daemon()` (so the daemon is ready) but before any IPC calls (so no state is created on failure).

**Config loading**: The gate needs `ClarityGateConfig` before connecting to the daemon. `run_headless` already loads `Config` via `Config::load()` for other pre-IPC decisions. The gate reads `config.strategy.clarity_gate` from the same loaded config.

### Gate Bypass Conditions

The clarity gate is skipped when:
- `--plan` is provided (the plan IS the specification)
- `--skip-clarity-gate` flag is set (power user escape hatch)
- `clarity_gate.enabled` is `false` in config

### Evaluation Criteria

The Sonnet prompt evaluates three dimensions:

1. **Specificity**: Does the goal describe a concrete, bounded change with identifiable artifacts?
   - Pass: "Add a /version slash command that displays the crate version"
   - Fail: "improve the system" / "make things better"

2. **Acceptance Criteria**: Can you objectively determine when the goal is complete?
   - Pass: "displays version string matching Cargo.toml" (testable)
   - Fail: "works better" / "is more reliable" (subjective)

3. **Scope**: Is the goal a single coherent concern, not a wishlist?
   - Pass: "Add SQLite persistence to the todo CLI"
   - Fail: "Refactor the TUI, add auth, and migrate the database"

### Prompt Design

```
You are a goal clarity evaluator for an autonomous software engineering system.
The system will decompose this goal into a multi-level plan and execute it
without human intervention. Vague goals waste significant resources.

Evaluate this goal for autonomous execution readiness:

<goal>
{goal_text}
</goal>

Rate each dimension 1-5:
- specificity: Does it describe a concrete, bounded change?
- acceptance: Can completion be objectively verified?
- scope: Is it a single coherent concern (not a wishlist)?

Respond with JSON only:
{
  "specificity": { "score": 1-5, "reason": "one sentence" },
  "acceptance": { "score": 1-5, "reason": "one sentence" },
  "scope": { "score": 1-5, "reason": "one sentence" },
  "improved_goal": "If any dimension < 3, suggest a concrete improved version of the goal."
}
```

### Response Schema

```rust
#[derive(Debug, Deserialize)]
struct DimensionScore {
    score: u8,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ClarityResponse {
    specificity: DimensionScore,
    acceptance: DimensionScore,
    scope: DimensionScore,
    #[serde(default)]
    improved_goal: Option<String>,
}

impl ClarityResponse {
    /// Pass if all dimensions meet the threshold (computed client-side, not by the LLM)
    fn passes(&self, min_score: u8) -> bool {
        self.specificity.score >= min_score
            && self.acceptance.score >= min_score
            && self.scope.score >= min_score
    }
}
```

### CLI Output on Failure

```
$ loopr run "make things better"

Goal rejected: too vague for autonomous execution.

  Specificity: 1/5 - No concrete deliverable identified
  Acceptance:  1/5 - No way to verify completion
  Scope:       2/5 - Unbounded improvement request

Suggested goal: "Add a /health endpoint that returns JSON with uptime and version"

Options:
  1. Be specific:    loopr run "Add a /version command that prints the crate version"
  2. Provide a plan: loopr run --plan docs/my-feature.md
  3. Use the TUI:    loopr  (refine the goal interactively)
  4. Bypass:         loopr run --skip-clarity-gate "make things better"
```

### Config

Add to `StrategyConfig`:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClarityGateConfig {
    /// Enable/disable the clarity gate (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Model for clarity evaluation (default: claude-sonnet-4-6)
    #[serde(default = "default_clarity_model")]
    pub model: String,
    /// Minimum score per dimension to pass (default: 3)
    #[serde(default = "default_min_score")]
    pub min_score: u8,
    /// API key env var (default: ANTHROPIC_API_KEY)
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
}
```

### Implementation Plan

**Phase 1: Add ClarityGate module and config**
- Create `src/clarity.rs` with `ClarityGate` struct
- Add `ClarityGateConfig` to `StrategyConfig`
- Add `--skip-clarity-gate` flag to `Run` command in `src/cli/mod.rs`
- Implement the async `evaluate()` method using `reqwest` (non-streaming, single call)
- Parse response JSON into `ClarityVerdict`

**Phase 2: Wire into run_headless**
- In `src/cli/dispatch.rs::run_headless()`, insert gate check after `ensure_daemon()` and before `coordinator.set_goal`
- Skip gate when `--plan`, `--skip-clarity-gate`, or `!config.clarity_gate.enabled`
- On failure: print formatted feedback, return exit code 3
- On pass: continue existing flow

**Phase 3: Error handling and edge cases**
- API key missing: warn and skip gate (don't block on config issues)
- API call fails (network, rate limit): warn and skip gate (fail open)
- Malformed JSON response: try to extract partial scores; if unparseable, warn and skip gate
- Timeout: 10s hard timeout on the Sonnet call
- LLM returns scores outside 1-5 range: clamp to 1-5
- LLM returns valid JSON but missing fields: serde default (score: 0, reason: "") - will fail gate, which is safe
- Goal contains prompt injection attempts: the gate prompt has no tool use or side effects, so injection is harmless (worst case: a vague goal passes)
- Very long goals (>10K chars): truncate to first 2000 chars for the gate call to control token costs

## Alternatives Considered

### Alternative 1: Run clarity gate in the daemon as a handler
- **Description:** Add a `coordinator.validate_goal` IPC method
- **Pros:** Centralized validation, accessible from TUI too
- **Cons:** Adds IPC round-trip, creates state before validation, daemon needs its own reqwest client separate from agent sessions
- **Why not chosen:** The gate is a CLI-side pre-check. It should fail before any state is created. The daemon shouldn't know about it - it's a UX concern, not an orchestration concern.

### Alternative 2: Use the existing DocValidator
- **Description:** Repurpose the DocValidator (which validates Plan/Spec documents) to also validate goals
- **Pros:** Reuses existing LLM validation infrastructure
- **Cons:** DocValidator uses sync `ureq` (not async), evaluates document quality not goal clarity, and has a completely different prompt/schema. Shoehorning goal validation into it would conflate two concerns.
- **Why not chosen:** Different validation target, different prompt, different response schema. Better as a standalone module.

### Alternative 3: Regex/heuristic gate instead of LLM
- **Description:** Check goal length, presence of verbs, keyword matching
- **Pros:** Free, instant, no API dependency
- **Cons:** Trivially bypassed ("Build a good system with tests"), can't evaluate semantic clarity, high false-positive rate
- **Why not chosen:** The value of the gate is semantic understanding. "Build a CLI todo app" and "build something" are syntactically similar but semantically different. Only an LLM can distinguish them.

### Alternative 4: No gate - let the pipeline self-correct
- **Description:** Accept any goal, let Coverage Evaluator and NeedHelp handle bad goals
- **Pros:** Simpler, no extra code
- **Cons:** Burns 10-30 Opus calls ($0.50-$2.00) before reaching NeedHelp on a goal that a single Sonnet call ($0.001) could have rejected. The original design doc explicitly rejected this approach.
- **Why not chosen:** Fail fast, fail cheap. The math is clear.

## Technical Considerations

### Dependencies

- `reqwest` (already a dependency - used by agent LLM client)
- `ANTHROPIC_API_KEY` env var (same as all other LLM calls)
- No new external crates needed

### Performance

- Single non-streaming Sonnet call: ~1-2s latency, ~$0.001 cost
- `max_tokens: 512` keeps response fast and cheap
- No retry logic - if the call fails, skip the gate (fail open)

### Testing Strategy

- **Unit tests**: Parse various `ClarityVerdict` JSON responses (pass, fail, malformed)
- **Unit tests**: Gate bypass logic (--plan, --skip-clarity-gate, config disabled)
- **Unit tests**: Failure formatting (verify CLI output matches spec)
- **Integration test** (ignored, requires API key): Run gate against known-good and known-bad goals
- **Persona tie-in**: The VAGUE_USER and SILENT_USER persona fixtures from `tests/funnel.rs` provide natural test goals that should fail the gate

### Rollout Plan

- Ship behind `clarity_gate.enabled: true` (default on)
- `--skip-clarity-gate` as immediate escape hatch
- Monitor false-negative rate (valid goals rejected) via user feedback
- Tune prompt and `min_score` threshold based on real usage

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| False negatives (valid goals rejected) | Medium | Low | `--skip-clarity-gate` escape hatch; threshold tuning; err on the side of permissiveness |
| False positives (vague goals pass) | Low | Medium | Gate is first filter, not only defense; Coverage Evaluator and NeedHelp are downstream safety nets |
| Sonnet API unavailable | Low | Low | Fail open - skip gate, warn user, continue |
| Prompt drift (Sonnet behavior changes across versions) | Low | Low | Pin model version in config; prompt is simple and stable |
| Gate adds perceived latency to `loopr run` | Medium | Low | ~1-2s is acceptable for a command that runs for minutes/hours; communicate with a spinner |
| Goal passes gate but is still too vague for good decomposition | Medium | Medium | Gate is deliberately permissive (threshold 3/5); Coverage Evaluator and NeedHelp remain the deep safety nets |
| LLM scores are inconsistent across calls for the same goal | Low | Low | Single call, no retry - accept the verdict. Borderline goals can use `--skip-clarity-gate` |

## Open Questions

- [ ] Should the gate also be available as a standalone command (`loopr check-goal "..."`) for prompt testing without execution?
- [ ] Should passing goals log their scores (to a Learning or log file) for later analysis of gate effectiveness?
- [ ] Should the gate's `improved_goal` suggestion be offered as an interactive "use this instead? [Y/n]" prompt when stdout is a terminal?

## References

- Prior art: [2026-03-21-coverage-bubble-up-and-headless-mode.md](2026-03-21-coverage-bubble-up-and-headless-mode.md) (clarity gate section)
- `loopr run` implementation: `src/cli/dispatch.rs:112-238`
- Coordinator goal handlers: `src/daemon/handlers/coordinator.rs`
- LLM client patterns: `src/agents/llm_client.rs`
- Chat funnel persona tests: `tests/funnel.rs`
- Config structure: `src/config.rs`
- RWL Research: "Spend 30+ minutes on requirements discussion"
- Stripe Minions: Blueprint engine, outloop coding pattern
- Agent Debate synthesis: "Specification collapse kills agents"
