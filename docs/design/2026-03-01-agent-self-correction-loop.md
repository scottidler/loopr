# Design Document: Agent Self-Correction Loop & Advisory Review Gates

**Author:** Scott Idler + Claude
**Date:** 2026-03-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

When an LLM agent produces malformed JSON or calls a tool with invalid arguments, the current system counts the failure and escalates after a threshold. The LLM never sees its own error — it gets a fresh context on the next iteration with a generic "previous iteration failed" note. This document adds intra-iteration self-correction: when output is malformed, the error is formatted and fed back to the LLM within the same API call (as a multi-turn conversation) for immediate self-correction. It also softens the Reviewer gate so deterministic validation (tests, compile, lint) is the hard blocker while LLM review provides advisory feedback.

## Problem Statement

### Background

The current error handling stack:
- **Lifeguard** (`lifeguard.rs`): Tracks repeated errors in a sliding window. Escalates to `NeedHelp` after 3 identical errors in 10.
- **Parse failure counting**: The lifeguard's `record_parse_failure()` counts consecutive parse failures. After 3, escalates.
- **Previous summary**: Failed iterations append an error note to `previous_summary`, which appears in the next iteration's context.
- **Config error classification** (`live-run-fixes.md`): Config errors (tool not found, wrong binary) are excluded from lifeguard escalation.

What's missing: **the LLM never gets to see its specific error and try again within the same turn.** Every failure costs a full iteration: fresh context assembly, full LLM call, fresh prompt. For a simple JSON syntax error (missing comma, wrong field name), this is enormously wasteful.

### Problem

**Problem 1 — Parse failures waste full iterations:**

The Implementer's `run_iteration()` calls `parse_actions()`. If parsing fails:
1. Lifeguard records parse failure
2. Error appended to `previous_summary`
3. Iteration ends
4. Next iteration: full context rebuild, full LLM call ($0.01-0.03)
5. LLM sees "previous iteration had a parse error" but doesn't see the EXACT error or its malformed output
6. LLM may make the SAME syntax error again (no correction signal)

From live runs: the LLM commonly produces `"args": {}` (object instead of array), `"files": [...]` (wrong field name), or bare `done` without a `summary` field. Each of these burns a full iteration when a simple "your output had error X, please fix it" re-prompt would get the correct output immediately.

**Problem 2 — Tool errors waste full iterations:**

When a tool call fails (e.g., `cargo test` fails with a compilation error), the error is recorded and the iteration continues. But the next action in the sequence may depend on the failed tool. The LLM doesn't get to "react" to the tool failure within the same turn — it already committed its action sequence.

The SWE-agent research shows that "re-querying" the LLM after a tool failure (showing it the error and asking for a corrected action) dramatically improves recovery rates. Gemini CLI implements this as well.

**Problem 3 — Reviewer as hard gate creates bottlenecks:**

The Bundle pipeline is: `Proposed → Triaged → Reviewed → Accepted`. The Reviewer's verdict (`Approve`/`RequestChanges`/`Reject`) is a hard gate. If the Reviewer rejects, the Bundle goes to `Rejected` and the Implementer must retry from scratch.

But the Reviewer is an LLM — it can be pedantic, inconsistent, or wrong. In live runs, the Reviewer rejected a Bundle for "missing edge case tests" that were actually present (the Reviewer hallucinated about the test coverage). The Implementer then wasted 20 iterations re-implementing something that was already correct.

Meanwhile, the Integrator (deterministic) runs `cargo test`, `cargo clippy`, `cargo fmt --check` — and these ACTUALLY tell you if the code works. If the tests pass and the linter is clean, the code is functionally correct regardless of what the Reviewer thinks.

### Goals

- LLM agents can self-correct malformed output within the same API call (multi-turn re-prompt)
- Tool errors can trigger intra-turn re-prompting (show error, ask for corrected action)
- Max re-prompts per turn is bounded (prevent infinite correction loops)
- Reviewer becomes advisory: provides feedback Learnings but does NOT block the Bundle pipeline
- Deterministic validation (Integrator) remains the hard gate for code quality
- Backward-compatible: self-correction enhances existing agents without breaking them

### Non-Goals

- Full conversation history across iterations (Ralph Wiggum Loop principle: fresh context each iteration)
- LLM-based loop detection (Gemini CLI's Tier 3 — deferred to MVP5)
- Reviewer removal (the Reviewer still runs and provides valuable feedback)
- Changing the Integrator's validation logic
- Tool output manipulation (showing partial tool output for correction)

## Proposed Solution

### Overview

Two changes:

1. **Self-Correction Loop** — When `parse_actions()` fails or a tool call returns an error, the LLM gets a follow-up message with the error and a correction prompt within the same API call. Up to `max_requeries` (default 3) re-prompts per iteration.
2. **Advisory Review Gate** — The Reviewer produces feedback Learnings but the Bundle pipeline skips the hard `Reviewed → Accepted` gate. The Integrator (deterministic tests) becomes the sole hard gate.

### Change 1: Self-Correction Loop

**Concept:** Instead of a single LLM call per iteration (system + user → assistant response), the iteration becomes a multi-turn conversation:

```
Turn 1:  system + user → assistant (response)
         parse_actions(response) → Err("missing field 'summary' in Done action")
Turn 2:  system + user + assistant(response) + user(error) → assistant (corrected response)
         parse_actions(corrected) → Ok(actions)
         execute actions normally
```

This is a standard multi-turn API call — the same conversation context with additional messages appended. The LLM sees its own malformed output and the specific error, which is exactly the signal it needs to fix it.

**File:** `src/agents/implementer.rs` (and analogous changes in `coordinator.rs`, `researcher.rs`)

**Current `run_iteration()` flow:**
```rust
let messages = build_messages(system_prompt, user_message);
let response = self.llm.call(&messages).await?;
match parse_actions(&response) {
    Ok(actions) => execute(actions),
    Err(e) => { self.lifeguard.record_parse_failure(); /* iteration wasted */ }
}
```

**New `run_iteration()` flow:**
```rust
let mut messages = build_messages(system_prompt, user_message);
let mut requeries = 0;

loop {
    let response = self.llm.call(&messages).await?;

    match parse_actions(&response) {
        Ok(actions) => {
            self.lifeguard.reset_parse_failures();
            return self.execute_actions_with_correction(&mut messages, actions).await;
        }
        Err(parse_err) => {
            requeries += 1;
            if requeries > self.config.max_requeries {
                // Exceeded re-prompt budget — fall through to lifeguard
                self.lifeguard.record_parse_failure();
                return Err(parse_err);
            }

            // Append the failed response and error as new messages
            messages.push(Message::assistant(&response));
            messages.push(Message::user(&format!(
                "Your response could not be parsed as a valid JSON action array.\n\
                 Error: {}\n\n\
                 Please respond with ONLY a valid JSON array of actions. \
                 Do not include any text before or after the JSON.",
                parse_err
            )));
            // Loop: LLM gets another chance with its error visible
        }
    }
}
```

**Tool error correction within the same turn:**

```rust
async fn execute_actions_with_correction(
    &mut self,
    messages: &mut Vec<Message>,
    actions: Vec<AgentAction>,
) -> Result<IterationOutcome> {
    let mut remaining_corrections = self.config.max_requeries;

    for action in &actions {
        match execute_action(action, &self.ctx, &self.worktree_path).await {
            Ok(result) => {
                // Record success in summary
                self.record_action_result(action, &result);
            }
            Err(e) if is_correctable_error(&e) && remaining_corrections > 0 => {
                remaining_corrections -= 1;

                // Show the LLM its error and ask for a corrected action
                let correction_prompt = format!(
                    "The action `{}` failed with error:\n{}\n\n\
                     Please provide a corrected action as a JSON array with a single action.",
                    action_summary(action), e
                );

                messages.push(Message::assistant(
                    &serde_json::to_string(&actions)?
                ));
                messages.push(Message::user(&correction_prompt));

                let corrected_response = self.llm.call(messages).await?;
                match parse_actions(&corrected_response) {
                    Ok(corrected_actions) => {
                        // Execute corrected action(s)
                        for ca in &corrected_actions {
                            let result = execute_action(ca, &self.ctx, &self.worktree_path).await;
                            self.record_action_result(ca, &result.unwrap_or_else(|e|
                                ActionResult::ActionError(e.to_string())
                            ));
                        }
                    }
                    Err(_) => {
                        // Correction also failed — record error and continue
                        self.record_action_error(action, &e);
                    }
                }
            }
            Err(e) => {
                // Non-correctable error or no corrections remaining
                self.record_action_error(action, &e);
                if let Verdict::Escalate(reason) = self.lifeguard.record_error(&e.to_string()) {
                    return Ok(IterationOutcome::NeedHelp(reason));
                }
            }
        }
    }

    Ok(IterationOutcome::Continue(self.build_summary()))
}
```

**What's a "correctable error"?**

```rust
fn is_correctable_error(error: &eyre::Report) -> bool {
    let msg = error.to_string();
    // Parse/schema errors — LLM can fix these
    msg.contains("missing field")
        || msg.contains("unknown field")
        || msg.contains("invalid type")
        || msg.contains("expected array")
        || msg.contains("path escapes")       // wrong file path
        || msg.contains("unknown tool")        // wrong tool name
        || msg.contains("path traversal")      // ../.. attempt
}
```

Non-correctable errors (compilation failure, test failure, network error) are NOT re-prompted. These require the LLM to think about what went wrong at the code level, which happens in the next full iteration with the error in `previous_summary`.

**Re-prompt budget:** The `max_requeries` budget is shared between parse corrections and tool corrections within a single iteration. Parse corrections consume from the budget first (they happen before action execution). Remaining budget is available for tool error corrections. With `max_requeries: 3`, a worst case is: 1 parse correction + 2 tool corrections, or 0 parse corrections + 3 tool corrections. Separate budgets would add complexity without clear benefit — the total LLM calls per iteration is what matters for cost control.

**Ralph Wiggum Loop compatibility:** Multi-turn correction within a single iteration does NOT violate the RWL principle. The RWL says each iteration starts with fresh context (no memory of prior iterations). Corrections happen WITHIN an iteration — they're part of the same turn, not a new iteration. The context window grows by ~500 tokens per correction (the error + correction messages), well within the model's capacity.

**Config:**

```rust
// In AgentRoleConfig
pub struct AgentRoleConfig {
    // ... existing fields ...
    pub max_requeries: u32,    // default 3
}
```

### Change 2: Advisory Review Gate

**Current flow:**
```
Proposed → Triaged (Coordinator) → Reviewed (Reviewer: Approve/Reject) → Accepted (Coordinator)
                                         ↓
                                    Rejected (hard gate)
```

**New flow:**
```
Proposed → Triaged (Coordinator) → Accepted (Coordinator, after deterministic pre-check)
                                        ↑
                                   Reviewer runs in parallel, creates Learning (advisory)
```

**Mechanism:**

The Bundle pipeline changes:
1. Coordinator triages Bundle (`Proposed → Triaged`)
2. Reviewer is spawned (existing auto-start on `Triaged`)
3. **NEW:** Coordinator can transition `Triaged → Accepted` without waiting for Reviewer verdict
4. Reviewer's verdict creates a Learning (scoped to the Work, tagged `review:feedback`)
5. If Reviewer verdict is `RequestChanges`, the Learning contains specific feedback
6. If the Integrator's deterministic validation FAILS (tests, lint), the Bundle is rejected — this is the hard gate
7. If the Integrator passes AND the Reviewer had `RequestChanges`, the feedback Learning is available to the next Implementer iteration (if any) or to the Coordinator for process improvement

**FSM change:**

Add a new transition to `bundle_transitions()`:

```rust
// Coordinator can bypass review and accept directly
FsmTransition {
    from: BundleStatus::Triaged,
    to: BundleStatus::Accepted,
    allowed_roles: vec![Role::Coordinator],
},
```

This transition shortcuts the Reviewer step. The existing `Reviewed → Accepted` path still works for cases where the Coordinator wants to wait for review.

**Reviewer race condition:** If the Coordinator accepts a Bundle (`Triaged → Accepted`) while the Reviewer is still running, the Reviewer will attempt `Triaged → Reviewed` on a Bundle that is already `Accepted`. The transition fails (Bundle not in `Triaged` state). The Reviewer should handle this gracefully: log "Bundle already advanced past Triaged", create its feedback Learning anyway (the feedback is still valuable), and complete normally. This requires a small change in `reviewer.rs` to catch transition failures and continue rather than failing the session.

**Coordinator prompt update:**

```
When triaging a Bundle:
- You MAY accept a Bundle directly (Triaged → Accepted) if you have confidence in the Implementer
- The Reviewer still runs and provides feedback as Learnings
- The Integrator (deterministic: tests, lint, compile) is the hard quality gate
- If the Integrator rejects, the Bundle is rejected regardless of review status

When a Reviewer produces feedback (review:feedback Learning):
- Read the feedback on your next iteration
- If the feedback identifies real issues, consider creating a new Work to address them
- Do not block the pipeline waiting for perfect review scores
```

**Why this is safe:** The Integrator runs `cargo test`, `cargo clippy`, `cargo fmt --check` (or project-specific equivalents). These are deterministic, objective quality checks. If the code compiles, passes tests, and satisfies the linter, it is functionally correct. The Reviewer's subjective feedback (code style, architecture suggestions) is valuable for learning but should not block a working implementation.

### Interaction Between Changes

The self-correction loop and advisory review gate are independent but complementary:

- **Self-correction** reduces wasted iterations from parse/schema errors (fewer iterations needed per Work)
- **Advisory review** reduces wasted iterations from pedantic rejections (fewer retry cycles per Bundle)
- Together, they make the "Implementer → Bundle → Integration" pipeline significantly faster

### Data Model

| Change | Type | File |
|--------|------|------|
| `AgentRoleConfig.max_requeries` | New config field (`u32`, default `3`) | `config.rs` |
| `Triaged → Accepted` for Coordinator | New FSM transition | `bundle.rs` |
| `Message` struct | Existing (in `llm_client.rs`) — no change needed | N/A |
| Review feedback tag convention | Convention (`review:feedback` tag) | Reviewer prompt |

### Implementation Plan

**Phase 1: Self-correction for parse failures**
- Modify `run_iteration()` in `implementer.rs` to support multi-turn re-prompting on parse failure
- Add `max_requeries` to `AgentRoleConfig`
- Tests: parse failure triggers re-prompt, corrected output parsed successfully, max_requeries respected

**Phase 2: Self-correction for tool errors**
- Add `is_correctable_error()` classifier
- Add `execute_actions_with_correction()` to Implementer
- Apply same pattern to Coordinator and Researcher
- Tests: correctable error triggers re-prompt, non-correctable error falls through to lifeguard

**Phase 3: Advisory review gate**
- Add `Triaged → Accepted` transition for Coordinator
- Update Reviewer to always create feedback Learning (even on Approve)
- Update Coordinator prompt to use direct acceptance
- Tests: Coordinator accepts Bundle directly, Reviewer feedback becomes Learning, Integrator still rejects on test failure

**Phase 4: Integration**
- End-to-end test: Implementer with parse error self-corrects, Bundle accepted directly, Integrator validates
- Verify: Reviewer feedback available as Learning for next iteration
- Verify: lifeguard still escalates after max_requeries exceeded

## Alternatives Considered

### Alternative 1: Structured output / function calling

- **Description:** Use Anthropic's tool_use / structured output API to enforce JSON schema at the API level, eliminating parse failures entirely.
- **Pros:** Zero parse failures. Schema enforced by the API.
- **Cons:** Anthropic's tool_use API returns structured output but with different semantics (tool calls vs. free-form JSON arrays). Would require significant refactoring of the action parsing pipeline. Also doesn't help with tool execution errors.
- **Why not chosen:** The current JSON-array approach is well-tested and flexible. Self-correction handles the ~5% of responses that deviate from schema. Migrating to tool_use is a larger refactor that can be considered independently.

### Alternative 2: Remove Reviewer entirely

- **Description:** Delete the Reviewer agent. Only the Integrator (deterministic) validates code.
- **Pros:** Simplest. No LLM cost for review. Eliminates the bottleneck entirely.
- **Cons:** Loses subjective code quality feedback (architecture, naming, patterns). The Reviewer catches issues that tests don't: poor naming, code duplication, missing edge case handling that passes current tests but would fail future tests. These become Learnings that improve future Implementer iterations.
- **Why not chosen:** The Reviewer is valuable as an advisor. It just shouldn't be a blocker.

### Alternative 3: Retry with full fresh context (current approach, tuned)

- **Description:** Instead of multi-turn re-prompting, just make the next iteration's `previous_summary` more explicit about the exact error and the exact malformed output.
- **Pros:** Preserves the Ralph Wiggum Loop purity (truly fresh context each iteration). No multi-turn complexity.
- **Cons:** Each retry costs a full iteration ($0.01-0.03 in API cost, 10-30s latency). For a missing comma in JSON, this is absurdly expensive. The RWL principle exists to prevent context window bloat over many iterations — a 2-3 turn correction within a single iteration doesn't violate it.
- **Why not chosen:** The cost is too high. A simple JSON fix should not cost a full iteration.

### Alternative 4: Reviewer with configurable strictness

- **Description:** Instead of making the Reviewer advisory, add a `reviewer_strictness` config knob: `hard_gate` (current), `advisory` (proposed), `disabled`.
- **Pros:** User choice. Teams that want strict review keep it.
- **Cons:** Three modes to test and maintain. The `hard_gate` mode has the same bottleneck problem.
- **Why not chosen for MVP:** Advisory is the right default. If users want hard review gates, they can configure the Integrator's validation commands to include review-like checks (e.g., `cargo clippy -- -D warnings` catches many style issues deterministically). A strictness knob can be added later if there's demand.

## Technical Considerations

### Dependencies

No new crates. Multi-turn conversation is supported by the existing `AgentLlmClient` (it already accepts `Vec<Message>`).

### Performance

- **Self-correction:** Each re-prompt is an incremental API call (previous messages cached by Anthropic). Cost: ~$0.003 per correction (only the new assistant + user messages are uncached). Much cheaper than a full iteration ($0.01-0.03).
- **Advisory review:** Reviewer still runs (same cost). But the pipeline doesn't wait for it. Net: faster throughput, same review cost.
- **Token budget:** Multi-turn messages within an iteration grow the context. With `max_requeries: 3`, worst case adds ~2000 tokens (3 error + 3 correction messages). Well within the model's context window.

### Security

- Self-correction messages are system-generated (error messages from the parser/executor). No user-controlled injection vector.
- The LLM cannot "trick" the parser by producing output that changes the correction prompt — the correction prompt is a fixed template with the error string interpolated.
- `is_correctable_error()` is conservative — only known schema/path errors trigger correction. Compilation errors, test failures, and other complex errors do NOT trigger re-prompting (they require full-iteration reasoning).

### Testing Strategy

**Unit tests:**
- Parse failure → re-prompt → corrected output parsed
- Parse failure → re-prompt × 3 → all fail → lifeguard escalation
- Tool error (correctable) → re-prompt → corrected action executed
- Tool error (non-correctable) → no re-prompt → lifeguard records error
- `is_correctable_error()` classification for known error patterns
- Bundle `Triaged → Accepted` transition valid for Coordinator
- Reviewer creates feedback Learning on all verdicts

**Integration tests:**
- Full iteration: LLM returns malformed JSON → self-corrects → actions executed → iteration succeeds
- Advisory review: Bundle accepted without Reviewer → Integrator validates → Published
- Advisory review: Bundle accepted → Integrator fails → Bundle rejected (hard gate works)
- Reviewer feedback available as Learning on next Coordinator iteration

### Rollout Plan

- Self-correction: enabled by default (`max_requeries: 3`). Set to 0 to disable.
- Advisory review: enabled by adding `Triaged → Accepted` transition. Not feature-flagged — the Coordinator simply uses the new transition when it judges appropriate. The existing `Reviewed → Accepted` path still works.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Self-correction loop adds latency to successful iterations | Low | Low | Re-prompting only triggers on errors. Successful parses take the fast path (zero overhead). |
| LLM produces worse output on correction (degrades rather than fixes) | Low | Medium | `max_requeries` bounds the correction attempts. If all 3 fail, lifeguard escalates as before. |
| Advisory review lets low-quality code through | Medium | Medium | Integrator's deterministic checks (tests, lint) catch functional issues. Style/architecture feedback still captured as Learnings for future iterations. |
| Multi-turn messages grow context beyond model limit | Very Low | Low | Max 3 corrections × ~500 tokens each = ~1500 tokens. Model context is 200K+. |
| Reviewer feedback Learnings ignored by Coordinator | Medium | Low | Coordinator prompt explicitly mentions review feedback Learnings. Context builder includes them with `review:feedback` tag. |

## Open Questions

- [ ] Should self-correction messages include the LLM's malformed output in full, or truncated to the first 500 chars?
- [ ] Should tool error correction re-execute the corrected action immediately, or queue it for the next iteration?
- [ ] Should the advisory review be configurable per-project (some teams may want hard review gates)?
- [ ] Should the Reviewer auto-start be delayed (e.g., only start after the Integrator has begun validation) to avoid wasted review of code that fails tests?
- [ ] When a corrected action introduces NEW subsequent actions (e.g., a corrected WriteFile followed by a new Commit), should these be appended to the current iteration's action list or deferred to the next iteration?
- [ ] Should the `AgentLlmClient` be updated to support incremental message appending (currently it takes `Vec<Message>` per call), or is the current API sufficient?

## References

- [SWE-agent agents.py](https://github.com/SWE-agent/SWE-agent/blob/main/sweagent/agent/agents.py) — `max_requeries=3`, format error re-prompting
- [Gemini CLI LoopDetectionService](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/services/loopDetectionService.ts) — Multi-tier loop detection
- [Why Do Multi-Agent LLM Systems Fail?](https://arxiv.org/html/2503.13657v1) — 14 failure modes including "format adherence" and "verification bottleneck"
- `docs/design/2026-03-01-agent-runtime-bugs.md` — Circuit breaker (current parse failure handling)
- `docs/design/2026-02-26-multi-level-rwl.md` — Integrator as deterministic task, Bundle FSM
- `docs/design/2026-02-26-implementer-reviewer-agents.md` — Reviewer agent, Bundle lifecycle
- `src/agents/lifeguard.rs` — Current error tracking and escalation
- `src/agents/implementer.rs` — Current `run_iteration()` flow
- `src/domain/bundle.rs` — Bundle FSM transitions
- `docs/design/2026-03-01-coordinator-override-sla-recovery.md` — SLA override (interacts: self-correction reduces SLA breaches)
- `docs/design/2026-03-01-pull-based-work-queue.md` — Pull-based workers (interacts: advisory review accelerates worker throughput)
