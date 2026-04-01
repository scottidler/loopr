# Design Document: E2E Hardening - Phase-Scoped Validation and Reviewer Resilience

**Author:** Scott A. Idler
**Date:** 2026-03-31
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Three issues surfaced during the first python-todo E2E run: (1) validation commands are global but execution is incremental, causing an infinite rejection loop when validation references artifacts from future work items, (2) the Reviewer agent crashes on the first JSON parse failure with no retry, despite the Implementer, Researcher, and Coordinator all having retry loops, and (3) the Coordinator cannot distinguish structural failures (context overflow) from transient ones (API timeout), so it retries pointlessly. This document proposes phase-scoped validation, reviewer parse-retry, and error classification to close all three.

## Problem Statement

### Background

The python-todo E2E run (2026-03-31, v0.1.31) timed out after 900s with only 1 of 3 work items completed. Two bugs were found and fixed (system prompt truncation, validation deadlock). But the fixes were tactical - a bash short-circuit for validation and a hard error for prompt overflow. The underlying architectural issues remain.

### Problem 1: Validation Rejection Loop

The Integrator runs a single global list of `validation_commands` from `loopr.yml` after every tick merge. When Work 1 (todo.py) merged, the Integrator ran `pytest test_todo.py` - but test_todo.py is Work 3, which hasn't been created yet. Result: Work 1's tick always fails validation, the bundle is rejected, the work resets to Ready, and the cycle repeats indefinitely. Dependents never start because Work 1 never reaches Integrated.

The current workaround (`test -f test_todo.py && ... || test ! -f test_todo.py`) pushes the problem onto the manifest author. Every E2E target script must anticipate which files exist at which stage and write conditional bash. This doesn't scale.

### Problem 2: Reviewer Parse Failure

The Reviewer agent calls the LLM once, attempts to parse the response as `ReviewResult` JSON, and returns `Err` on failure. No retry, no format correction. The Implementer, Researcher, and Coordinator all have `max_requeries` retry loops (default 3) with self-correction prompts. The Reviewer config even defines `max_requeries: 3` but never uses it.

During the E2E run, 7 of 8 reviewers failed to parse because the system prompt was truncated (now fixed). But even with perfect prompts, LLMs occasionally produce malformed JSON. One bad response should not kill a review.

### Problem 3: Error Classification

When `ContextBuilder::build()` returns a hard error on context overflow, the error propagates correctly through the agent -> executor -> session pipeline. But the Coordinator sees it as a generic agent failure. It cannot distinguish "context window too large" (structural, requires user action) from "API timeout" (transient, worth retrying). This affects how the Coordinator responds - retrying a context overflow is pointless.

### Goals

- Phase-scoped validation: each phase defines what to validate, run only the relevant commands at tick time
- Reviewer retry: reuse the proven `max_requeries` pattern from Implementer/Researcher
- Error classification: tag agent failures with a category so the Coordinator can make informed decisions

### Non-Goals

- Work-level validation commands (too granular - phases are the right abstraction for integration checkpoints)
- Reviewer multi-iteration loops (the reviewer is one-shot by design - it reviews a bundle, not iterates on code)
- Automatic context reduction (splitting work or pruning learnings when context overflows - future work)
- Removing the bash short-circuit from existing E2E targets (backwards compatible, leave in place)

## Proposed Solution

### Overview

Three independent changes (no ordering dependency between them, but implementation is easier in the order listed):

1. Add `validation-commands` to `ManifestPhase` and `Phase` domain object. The Integrator resolves tick bundles back to their parent phases and runs the union of global + phase-scoped commands.
2. Add a parse-retry loop to `ReviewerAgent::run()` using the same `max_requeries` pattern as Implementer.
3. Introduce an `AgentErrorKind` enum to classify failures. Attach it to session error state so the Coordinator can dispatch on it.

### 1. Phase-Scoped Validation

#### Manifest Changes

Add optional `validation-commands` to `ManifestPhase`:

```yaml
phases:
  - title: "Core model"
    validation-commands:
      - "python -c 'import todo'"
    works:
      - key: "todo-model"
        # ...
  - title: "CLI and tests"
    validation-commands:
      - ".venv/bin/python -m pytest test_todo.py -v"
    works:
      - key: "cli-entry"
        # ...
      - key: "test-suite"
        # ...
```

#### Domain Changes

Add `validation_commands: Vec<String>` to `Phase` struct:

```rust
// src/domain/phase.rs
pub struct Phase {
    // ... existing fields ...
    #[serde(default)]
    pub validation_commands: Vec<String>,
}
```

Add `validation_commands` to `ManifestPhase` (kebab-case for YAML):

```rust
// src/manifest.rs
pub struct ManifestPhase {
    pub title: String,
    pub description: String,
    pub works: Vec<ManifestWork>,
    #[serde(default, rename = "validation-commands")]
    pub validation_commands: Vec<String>,
}
```

Thread the field through `parse_manifest()` into the resolved `Phase`. When the Coordinator creates phases via LLM decomposition (not manifest), `validation_commands` defaults to empty vec - global commands still apply.

**Important constraints:**

- This design only avoids the rejection loop when works are split across multiple phases with appropriate validation per phase. The current `python-todo.yaml` has a single phase containing all three works. It would need to be restructured into two phases (core model + CLI/tests) for phase-scoped validation to help. Single-phase manifests still fall back to global validation behavior.
- Even with multi-phase splitting, the problem can recur *within* a phase. If cli-entry and test-suite are both in Phase 2 and Phase 2's validation is "pytest test_todo.py," then when cli-entry merges before test-suite, Phase 2 validation runs and fails because test_todo.py doesn't exist yet. The manifest author must ensure phase validation only references artifacts that exist once *any* work in that phase merges - not just after *all* works complete. In practice this means: either put phase validation commands that reference specific files on the phase where that file is produced, or use conditional checks (the bash short-circuit pattern).

**Future direction: two-tier validation.** The intra-phase limitation exists because `validation-commands` run on every tick (every bundle merge). A cleaner long-term model would distinguish two concepts: (1) **Tick validation** - lightweight, runs on every merge (syntax checks, compile checks), and (2) **Phase gate validation** - heavyweight, runs only when all works in a phase reach Integrated, serving as the gate before the phase transitions to Complete. This would eliminate the intra-phase trap entirely but requires the Integrator to track phase-completion state, which is a larger change. Out of scope for this doc.

#### Integrator Changes

Both validation paths must be updated:
- `IntegratorAgent::run_cycle()` in `src/agents/integrator.rs` (the agent-based path)
- `handle_integrator_validate()` in `src/daemon/handlers/integrator.rs` (the RPC-based path)

Both currently call `run_validation_commands()` with the global config. Both need to call `effective_validation_commands()` to resolve phase-scoped commands first. Extract the shared function so both paths use it.

In `run_cycle()`, after sealing the tick and before running validation:

1. Collect the bundle IDs in the tick
2. Resolve each bundle -> work -> phase_id
3. Look up each phase's `validation_commands`
4. Build the effective command list: `global_commands + phase_commands` (deduplicated)
5. Run the effective list through `run_validation_commands()`

```rust
// src/agents/integrator.rs - free function, usable from both agent and daemon handler paths
fn effective_validation_commands(
    global_commands: &[String],
    bundle_ids: &[String],
    stores: &Stores,
) -> Vec<String> {
    let mut commands: Vec<String> = global_commands.to_vec();
    let mut seen: HashSet<String> = commands.iter().cloned().collect();

    let Ok(bundles) = stores.read_bundles() else { return commands };
    let Ok(works) = stores.read_works() else { return commands };
    let Ok(phases) = stores.read_phases() else { return commands };

    for bid in bundle_ids {
        if let Some(bundle) = bundles.get(bid) {
            if let Some(work) = works.get(&bundle.work_id) {
                if let Some(phase) = phases.get(&work.phase_id) {
                    for cmd in &phase.validation_commands {
                        if seen.insert(cmd.clone()) {
                            commands.push(cmd.clone());
                        }
                    }
                }
            }
        }
    }

    commands
}
```

If global `validation_commands` is empty and no phase commands are defined, skip validation entirely (current default behavior for disabled integrator).

### 2. Reviewer Parse Retry

The current reviewer calls `self.llm.call()` once and fails on any parse error. Replace with a retry loop: on parse failure, append the bad response and a correction prompt to the message history, then call `call_with_history()` again. This is the same pattern the Implementer uses at `implementer.rs:254-296`:

```rust
// src/agents/reviewer.rs - in run()
let review = {
    // Build message history for multi-turn retry.
    // call_with_history(system_prompt, messages) takes the system prompt
    // separately - messages contain only user/assistant turns.
    let mut messages = vec![
        ChatMessage::user(&assembled.user_message),
    ];
    let mut requeries = 0;

    loop {
        let response = self.llm
            .call_with_history(&assembled.system_prompt, &messages)
            .await?;
        self.ctx.log.write_iter_file(
            requeries,
            Some(&self.bundle_id),
            &assembled.system_prompt,
            &assembled.user_message,
            &response,
        );

        match parse_review_result(&response, &self.ctx.log) {
            Ok(result) => break result,
            Err(parse_err) => {
                requeries += 1;
                if requeries > self.config.max_requeries {
                    return Err(parse_err.wrap_err("reviewer exhausted parse retries"));
                }
                self.ctx.warn(&format!(
                    "parse attempt {}/{} failed: {}",
                    requeries, self.config.max_requeries, parse_err
                ));
                // Truncate bad response to avoid context growth -
                // full garbage responses can be thousands of tokens.
                let truncated: String = response.chars().take(200).collect();
                messages.push(ChatMessage::assistant(&truncated));
                messages.push(ChatMessage::user(&format!(
                    "Your response (starting with: {:?}...) could not be parsed \
                     as valid JSON. Error: {}\n\n\
                     Respond with ONLY a valid JSON object matching the schema \
                     in the system prompt. No markdown, no prose.",
                    &truncated[..truncated.len().min(100)],
                    parse_err,
                )));
            }
        }
    }
};
```

The `LlmClient` trait already defines `call_with_history(system_prompt, messages)` with a default impl that delegates to `call()`. The Reviewer currently uses `call()` directly (reviewer.rs:131) - switching to the multi-turn method requires no trait changes, just changing the call site. The default impl extracts the last user message, so even mock LLMs that only implement `call()` will work without modification.

**Bundle aftermath on failure:** If the reviewer exhausts retries, `run()` returns `Err` and the session is marked Failed. The bundle stays at Triaged - it is not explicitly rejected. The Coordinator assigns reviewers via `AssignAgent` actions, so it needs to detect stuck Triaged bundles (via its monitoring loop) and re-assign a new reviewer. This is existing Coordinator behavior - no change needed here, but it's worth verifying during testing.

**Message growth:** Each retry appends a truncated snippet (200 chars) of the bad response plus a correction prompt. This prevents context accumulation from garbage responses - important given that context overflow was one of the bugs that motivated this design. The correction prompt includes the parse error message to help the LLM understand what went wrong.

### 3. Error Classification

Define a typed error enum using `thiserror` for classifiable agent failures, alongside a serializable kind enum for session state:

```rust
// src/agents/error.rs

/// Typed agent errors for eyre downcasting at the executor level.
/// Returned from agent internals; auto-converts to eyre::Report via Into.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("assembled context ({tokens} tokens) exceeds model input limit ({limit} tokens)")]
    ContextOverflow { tokens: usize, limit: usize },

    #[error("exhausted {attempts} parse retries")]
    ParseExhausted { attempts: usize },
}

/// Serializable error kind for session state and Coordinator dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorKind {
    ContextOverflow,
    ParseExhausted,
    LlmTransient,
    ToolFailure,
    Unknown,
}
```

At error sites, return the typed error (auto-converts to eyre):

```rust
// In context.rs - build()
if token_estimate > MAX_INPUT_TOKENS {
    return Err(AgentError::ContextOverflow {
        tokens: token_estimate,
        limit: MAX_INPUT_TOKENS,
    }.into());
}

// In reviewer.rs - retry exhaustion
return Err(AgentError::ParseExhausted {
    attempts: self.config.max_requeries,
}.into());
```

At the executor, use eyre's `downcast_ref` to recover the typed error - no string matching for errors we control:

```rust
// In executor error path
let error_kind = if let Some(agent_err) = err.downcast_ref::<AgentError>() {
    match agent_err {
        AgentError::ContextOverflow { .. } => AgentErrorKind::ContextOverflow,
        AgentError::ParseExhausted { .. } => AgentErrorKind::ParseExhausted,
    }
} else {
    // Heuristic fallback for external crate errors (reqwest, etc.)
    let err_str = format!("{err:?}");
    if err_str.contains("status: 429") || err_str.contains("timed out") {
        AgentErrorKind::LlmTransient
    } else {
        AgentErrorKind::Unknown
    }
};
session.error_kind = Some(error_kind);
```

Attach to `AgentSession`:

```rust
// src/agents/mod.rs - in AgentSession (line 316)
pub error_kind: Option<AgentErrorKind>,
```

This gives compile-time safety for errors we control (`AgentError` variants) and falls back to string matching only for errors from external crates (reqwest timeouts, API rate limits) where we can't control the error type. Adding new `AgentError` variants automatically forces the match arm to be updated.

The Coordinator can then dispatch:

- `ContextOverflow` - transition work to Failed with a learning explaining the structural limit. Don't retry - the user needs to reduce scope or split the work.
- `ParseExhausted` - log and move on. The retry loop already tried `max_requeries` times.
- `LlmTransient` - worth retrying after backoff. The work stays Ready for the next worker cycle.
- `ToolFailure` - inspect tool error, may retry
- `Unknown` - current behavior (lifeguard after repeated failures)

## Alternatives Considered

### Alternative 1: Work-Level Validation Commands

- **Description:** Put `validation-commands` on each Work item instead of Phase
- **Pros:** Maximum granularity
- **Cons:** Too noisy. Most works don't need their own validation - they share a phase-level integration checkpoint. Adds cognitive load to manifest authoring.
- **Why not chosen:** Phase is the natural integration boundary. Works within a phase are validated together.

### Alternative 2: Reviewer Model Fallback

- **Description:** On parse failure, retry with a different model (e.g., fall back from Sonnet to Opus for better instruction following)
- **Pros:** Could fix cases where the model genuinely can't follow the schema
- **Cons:** Adds model-switching complexity, cost implications, the parse-retry alone should handle most cases
- **Why not chosen:** Premature. Try parse-retry first; revisit if parse exhaustion rate is still high.

### Alternative 3: Structured Output (JSON Mode)

- **Description:** Use the LLM's native JSON mode / structured output feature instead of parsing free-form text
- **Pros:** Eliminates parse failures entirely
- **Cons:** Not all providers support it identically. Claude's tool_use could work but changes the prompt/response contract significantly.
- **Why not chosen:** Worth investigating in a separate design doc. Parse-retry is the pragmatic fix now.

## Technical Considerations

### Dependencies

- No new external dependencies
- Phase validation requires `Phase` records to be queryable by ID from the Integrator (already possible via TaskStore indexed fields)

### Performance

- Parse retry adds at most `max_requeries` (3) extra LLM calls per review, only on failure. Normal path is unchanged.
- Phase validation lookup is O(bundles_in_tick * 1 store lookup per bundle) - negligible.

### Testing Strategy

**Phase-scoped validation:**
- Unit test: `effective_validation_commands()` with global + phase commands, deduplication, empty phases
- Unit test: `ManifestPhase` deserialization with and without `validation-commands`
- E2E: python-todo manifest with phase-scoped validation replacing the bash short-circuit

**Reviewer retry:**
- Unit test: mock LLM returns bad JSON then good JSON on second call - verify retry works
- Unit test: mock LLM returns bad JSON `max_requeries + 1` times - verify exhaustion error
- Existing tests: `test_run_reviewer_bad_response` continues to verify single-failure behavior

**Error classification:**
- Unit test: verify each error kind propagates correctly through executor -> session
- Integration test: coordinator receives `ContextOverflow` and transitions work to Failed with explanatory learning

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Phase validation commands not found (empty phase_id on old works) | Low | Low | Fall back to global commands only when phase lookup returns None |
| Reviewer retry loop masks a deeper prompt problem | Medium | Medium | Log every retry with full response for debugging. If retry rate is high, the prompt needs fixing - don't just rely on retries. |
| Error classification doesn't cover a failure mode | Low | Low | `Unknown` catch-all preserves current behavior for unclassified errors |
| Tick contains bundles from multiple phases with conflicting validation | Low | Medium | Union of all phase commands runs all checks. If commands conflict (e.g., phase 1 syntax check vs phase 2 full test suite), both run. This is correct - the tick merges all bundles, so all relevant checks should pass. |
| LLM-generated phases (not manifest) have no validation_commands | High | None | `validation_commands` defaults to empty vec via `#[serde(default)]`. Global commands still apply. This is the expected path for non-manifest runs. |
| Intra-phase validation references artifacts from sibling works not yet merged | Medium | Medium | Manifest author must ensure phase validation only checks artifacts that exist after any work in the phase merges. Document this constraint. Future: "phase-complete validation" that runs only when all works in a phase are Done. |
| Reviewer fails, bundle stays at Triaged indefinitely | Low | High | Coordinator's monitoring loop should detect Triaged bundles with failed reviewer sessions and re-assign. Verify this works in E2E. |

## Implementation Plan

### Step 1: Reviewer Parse Retry
Smallest change, highest immediate value. Wire `max_requeries` into `ReviewerAgent::run()`. Verifiable with existing mock tests.

### Step 2: Phase-Scoped Validation
Add `validation-commands` to ManifestPhase, Phase domain, and Integrator resolution. Restructure python-todo manifest into two phases. The updated `python-todo.yaml` would look like:

```yaml
phases:
  - title: "Core model"
    description: "Data model with JSON persistence"
    validation-commands:
      - ".venv/bin/python -c 'import todo'"
    works:
      - key: "todo-model"
        # ... (unchanged)
  - title: "CLI and test suite"
    description: "Command-line interface and pytest coverage"
    validation-commands:
      - ".venv/bin/python -m pytest test_todo.py -v"
    works:
      - key: "cli-entry"
        # ... dependencies: ["todo-model"]
      - key: "test-suite"
        # ... dependencies: ["todo-model"]
```

Run E2E to verify. The global `validation_commands` in `loopr.yml` can be set to empty (or omitted) since phase-level commands now cover it.

### Step 3: Error Classification
Add `AgentErrorKind`, wire through executor, teach Coordinator to dispatch on it. This is the most invasive change and benefits from the first two being stable.

## Open Questions

- [x] ~~Should the Reviewer use `call_with_history()` or can we reuse `call()` with a modified prompt on retry?~~ Resolved: `LlmClient` trait already has `call_with_history()` with a default impl. Switch the call site.
- [ ] Should phase validation commands *replace* global commands or *augment* them? Current proposal is augment (union). Replacing could be useful if a phase needs a lighter check (e.g., syntax-only), but the global commands represent project-wide invariants that should always hold. Leaning toward augment, with a `skip-global-validation: true` phase-level escape hatch if needed later.
- [ ] Is `Failed` the right status for context overflow, or do we need a distinct terminal state? `Failed` currently means "agent tried and errored." That matches, but the Coordinator should create a learning with the specific error kind so the user knows the failure is structural, not a bug.

## References

- [E2E AAR - Claude](../2026-03-31-e2e-timeout-after-action-report-claude.md)
- [E2E AAR - Gemini](../2026-03-31-e2e-timeout-after-action-report-gemini.md)
- [Orchestration Spine Design](2026-02-25-orchestration-spine.md)
- [Multi-Level RWL Design](2026-02-26-multi-level-rwl.md)
