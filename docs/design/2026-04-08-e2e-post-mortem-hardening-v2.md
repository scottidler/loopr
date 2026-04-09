# Design Document: E2E Post-Mortem Hardening v2 (python-api Run)

**Author:** Scott A. Idler
**Date:** 2026-04-08
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Four targeted fixes addressing second-order failure modes observed in the python-api E2E run after
the v1 hardening design doc was implemented. The key discovery: the coordinator LLM **never runs
in GoalComplete state** - `apply_fsm_transition` returns early, making the v1 quality gate prompt
instruction dead code. The v1 fixes (max-abandon-ratio, cross-module signatures, AC grounding,
fix-then-retry) were all prompt-based and all failed. The root failures cascade: generation
hallucinated the schema, unit tests cemented the hallucination, the reviewer approved it, and the
coordinator declared victory without executing any quality check. The fix principle:
**safety-critical decisions get code enforcement, not prompts.**

## Problem Statement

### Background

The v1 hardening design doc (`2026-04-08-e2e-post-mortem-hardening.md`) shipped six prompt and code
fixes. A subsequent python-api E2E run exposed that four of those fixes - all prompt-based -
failed to change LLM behavior at the critical moments:

- **max-abandon-ratio** (Phase 2): 40% threshold prompt instruction in coordinator.pmt. The
  coordinator LLM never ran in GoalComplete state (see P1), so the instruction was dead code.
  5 of 13 works were abandoned (38.5% all-works, 41.7% terminal-only).
- **Cross-module signatures** (Phase 3): Reviewer caught call-site mismatches but not the deeper
  problem - unit tests internally consistent with a wrong implementation.
- **AC grounding rule** (Phase 4): The decomposer work.pmt instruction was in place. The
  implementer hallucinated `notes`/`favorite` columns instead of `tags` anyway.
- **Fix-then-retry** (Phase 5): Coordinator prompt rule was in place. The coordinator wrote
  "Creating fix Work and replacement" in the abandon reason string but never emitted the
  `create_work` actions.

The pattern: prompt-only fixes are unreliable for safety-critical control flow decisions. When the
LLM must choose between "declare success and stop" vs "do more work", it defaults to the easier
path.

### Problems

**P1 - GoalComplete fires without any quality check**

The max-abandon-ratio quality gate (v1 Phase 2) was added as a prompt instruction in
coordinator.pmt. However, the coordinator LLM **never runs in GoalComplete state**. The FSM
auto-transitions to GoalComplete via `apply_fsm_transition` (coordinator.rs:750-767), which
immediately deactivates the goal, persists state, and returns `IterationOutcome::Done` - exiting
the iteration before the LLM executes. On the next loop iteration, `is_terminal()` returns true
and the loop exits.

This means:
- The coordinator.pmt GoalComplete quality gate instruction (lines 21-26) is dead code
- The `build_fsm_footer` GoalComplete footer (lines 952-958) is dead code
- The `goal_abandon_ratio()` utility (generation.rs:191) is dead infrastructure

The hardcoded summary at line 764 - `"Goal complete: {} phases completed"` - is the only output.
The LLM never gets to count Done vs Abandoned works, because it never runs in that state.

In this E2E run, 5 of 13 works were abandoned (38.5%). Using the current denominator (all works
including non-terminal), this is below the 40% threshold. But counting only terminal works (the
meaningful measure at GoalComplete), the ratio is 5/12 = 41.7% - above the threshold. The
`goal_abandon_ratio()` function uses the all-works denominator, which makes the gate weaker than
intended.

**P2 - Coordinator hallucinated repair task creation**

The fix-then-retry rule (v1 Phase 5) tells the coordinator to create fix Works when it sees
CROSS-FILE BUG learnings. The coordinator wrote the intent in abandon reason strings ("Creating
fix Work and replacement") but its JSON action payload never contained `create_work` actions.
Between the abandonment at 01:32:48 and GoalComplete at 01:32:53, the coordinator issued zero
create_work actions. The promise was prose in a reason field, not an executable action.

The failure mode: the LLM conflates "describing what it should do" with "doing it." Reason strings
are free-text; actions require structured JSON with specific fields. The LLM took the easier path.

**P3 - Reviewer approved a unit test suite validating the wrong schema**

Work wk-yhhnz ("Test Suite - All 16 Unit Tests") was marked Done and passed review. The 16 unit
tests in `test_database.py` tested `notes`/`favorite` columns - matching the wrong implementation,
not the spec AC. The spec (sp-3j0eu) said `create_bookmark() returns a dict with keys id, title,
url, tags`. The implementation and tests both used `notes`/`favorite`. The reviewer approved
because:

1. The work's AC said "pytest test_database.py -v exits with return code 0"
2. The tests passed (exit 0)
3. The tests were internally consistent with `database.py`

The reviewer never checked whether the tests validated the spec's contract. It verified
consistency (tests match implementation) but not correctness (tests match spec). The cross-module
signature verification (v1 Phase 3) didn't help here because the signatures were consistent - they
were just wrong signatures.

**P4 - Implementer hallucinated schema despite AC grounding rule**

The AC grounding rule (v1 Phase 4) in work.pmt says: "Every acceptance criterion MUST be derivable
from the parent document's Contracts, Interfaces, or Outputs sections." The database implementer
ignored this and generated `notes: str`, `favorite: bool` instead of `tags: str`. The spec
explicitly defined `tags`. The decomposer may have correctly generated ACs referencing `tags`, but
the implementer wrote code using different field names.

This is a generation-time failure, not a decomposition failure. The implementer received correct AC
but wrote code using hallucinated field names. The AC grounding rule constrains the decomposer; it
does not constrain the implementer.

### Goals

- Enforce the abandon-ratio quality gate in code, not just prompt
- Prevent coordinator from claiming actions it didn't emit
- Enable the reviewer to verify tests against spec AC, not just work AC
- Constrain the implementer to use field names from the spec/work AC

### Non-Goals

- Rewriting the coordinator FSM (use existing states)
- Adding static analysis or AST parsing for generated code
- Changing the phase gate logic itself (Done OR Abandoned is correct for FSM purposes)
- Preventing the FSM from reaching GoalComplete state (the state still transitions; only the
  outcome changes from Done to NeedHelp when the gate fires)

## Proposed Solution

### Overview

Four independent fixes, two code-level and two prompt-level, applied in priority order. The
key principle: **safety-critical decisions get code enforcement; advisory decisions stay in
prompts.**

### Phase 1 - Code-Enforced Abandon-Ratio Gate at GoalComplete

**Problem:** The coordinator LLM never runs in GoalComplete state. The quality gate prompt
instruction is dead code. GoalComplete fires unconditionally via `apply_fsm_transition`.

**Fix:** Compute the abandon ratio in `apply_fsm_transition` itself, before returning
`IterationOutcome::Done`. If the ratio exceeds the threshold, return `IterationOutcome::NeedHelp`
instead, which causes the run loop to exit with an error (triggering the `need_help` escalation
path).

**1a. Quality gate in apply_fsm_transition**

In `src/agents/coordinator.rs`, modify the GoalComplete branch of `apply_fsm_transition`
(lines 750-767):

```rust
} else if new_state == CoordinatorFsmState::GoalComplete {
    // Complete current phase if transitioning from PhaseGate
    if coord_state.current_phase_id.is_some() {
        mark_phase_record_complete(stores, coord_state, prefix);
        coord_state.complete_phase();
    }
    coord_state.transition_to(CoordinatorFsmState::GoalComplete);

    // --- Quality gate: check abandon ratio before declaring success ---
    let ratio = goal_abandon_ratio_terminal(stores, &coord_state.goal_id);
    let max_ratio = stores.config.agents.coordinator.max_abandon_ratio;
    if ratio > max_ratio {
        let (done_count, abandoned_count, total) =
            goal_work_counts(stores, &coord_state.goal_id);
        let reason = format!(
            "Quality gate: {abandoned_count}/{total} works abandoned \
             ({:.0}% > {:.0}% threshold). {done_count}/{total} completed.",
            ratio * 100.0, max_ratio * 100.0,
        );
        tracing::warn!("{} {}", prefix, reason);
        // Do NOT deactivate the goal - it needs help, not completion
        persist_coordinator_state(stores, coord_state);
        return Some(IterationOutcome::NeedHelp(reason));
    }

    // Deactivate the goal - quality gate passed
    if let Ok(mut goals) = stores.write_coordinator_goals()
        && let Some(goal) = goals.values_mut().find(|g| g.id == coord_state.goal_id)
    {
        goal.deactivate();
    }
    persist_coordinator_state(stores, coord_state);
    return Some(IterationOutcome::Done(format!(
        "Goal complete: {} phases completed",
        coord_state.phases_completed.len()
    )));
}
```

**1b. Terminal-only abandon ratio**

The existing `goal_abandon_ratio()` includes non-terminal works in the denominator, which
dilutes the ratio (5/13 = 38.5% vs 5/12 = 41.7%). At GoalComplete, only terminal works matter.
Add a variant that counts only Done + Abandoned works:

```rust
pub fn goal_abandon_ratio_terminal(stores: &Stores, plan_id: &str) -> f64 {
    // Same as goal_abandon_ratio but denominator is Done + Abandoned only
    let all_works = collect_goal_works(stores, plan_id);
    let terminal: Vec<_> = all_works.iter()
        .filter(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
        .collect();
    let abandoned = terminal.iter()
        .filter(|w| matches!(w.status(), WorkStatus::Abandoned))
        .count();
    if terminal.is_empty() { return 0.0; }
    abandoned as f64 / terminal.len() as f64
}
```

**1c. Shared helper: collect_goal_works**

Extract the work-collection logic from the existing `goal_abandon_ratio()` into a reusable helper.
Both `goal_abandon_ratio()` and the new `goal_abandon_ratio_terminal()` call it:

```rust
/// Collect all Work items descended from a plan (Brief mode: direct children;
/// Full mode: via Spec -> Phase -> Work chain).
fn collect_goal_works(stores: &Stores, plan_id: &str) -> Vec<Work> {
    // This is the existing logic from goal_abandon_ratio lines 192-219,
    // extracted into its own function. Returns owned Work clones.
}

fn goal_work_counts(stores: &Stores, plan_id: &str) -> (usize, usize, usize) {
    let works = collect_goal_works(stores, plan_id);
    let done = works.iter().filter(|w| matches!(w.status(), WorkStatus::Done)).count();
    let abandoned = works.iter().filter(|w| matches!(w.status(), WorkStatus::Abandoned)).count();
    (done, abandoned, works.len())
}
```

**1d. Clean up dead code**

- Remove or simplify the GoalComplete section in coordinator.pmt (lines 21-26). The quality
  gate is now code-enforced; the prompt instruction is misleading since the LLM never executes it.
- The `build_fsm_footer` GoalComplete branch (lines 952-958) can be simplified to a no-op since
  the LLM never runs in that state.
- Retain `goal_abandon_ratio()` for non-terminal contexts (logging, status display) but use
  `goal_abandon_ratio_terminal()` for the gate decision.

**1e. CRITICAL: Two paths to GoalComplete**

There are TWO code paths that transition to GoalComplete, and the quality gate must cover both:

1. **ActivatePhase fallback** (lines 735-743): When `find_next_phase_to_activate` returns None,
   the ActivatePhase handler transitions directly to GoalComplete and deactivates the goal. This
   is the primary completion path (the comment at line 737 says so). It does NOT return early -
   it falls through to `None`, and the next iteration's `is_terminal()` check exits the loop.

2. **Explicit GoalComplete branch** (lines 750-767): When `check_fsm_transition` returns
   GoalComplete directly (e.g., Brief mode, timeout). Returns `IterationOutcome::Done` early.

The Phase 1a code snippet covers path 2. Path 1 also needs the quality gate:

```rust
} else {
    // No more phases to activate - check quality gate before declaring complete
    let ratio = goal_abandon_ratio_terminal(stores, &coord_state.goal_id);
    let max_ratio = stores.config.agents.coordinator.max_abandon_ratio;
    if ratio > max_ratio {
        let (done_count, abandoned_count, total) =
            goal_work_counts(stores, &coord_state.goal_id);
        let reason = format!(
            "Quality gate: {abandoned_count}/{total} works abandoned \
             ({:.0}% > {:.0}% threshold). {done_count}/{total} completed.",
            ratio * 100.0, max_ratio * 100.0,
        );
        tracing::warn!("{} {}", prefix, reason);
        // Transition to GoalComplete but do NOT deactivate
        coord_state.transition_to(CoordinatorFsmState::GoalComplete);
        persist_coordinator_state(stores, coord_state);
        return Some(IterationOutcome::NeedHelp(reason));
    }

    coord_state.transition_to(CoordinatorFsmState::GoalComplete);
    if let Ok(mut goals) = stores.write_coordinator_goals()
        && let Some(goal) = goals.values_mut().find(|g| g.id == coord_state.goal_id)
    {
        goal.deactivate();
    }
}
```

**Implementation note:** Consider extracting the quality gate check into a helper function
(`check_abandon_gate`) called from both paths to avoid duplication.

**Why this works:** The code gate fires in both code paths that transition to GoalComplete -
there is no way to bypass it. The LLM is not involved in the decision. The `NeedHelp` outcome
causes the run loop to exit with an error, which surfaces to the E2E runner and TUI as a failure
requiring human intervention.

**Edge case: NeedHelp + GoalComplete state**: When the gate fires, `coord_state.fsm_state` is
GoalComplete (terminal) but the goal remains active. On daemon restart, `is_terminal()` returns
true and exits immediately. The NeedHelp is the final outcome - the system correctly refuses to
proceed without human intervention. The daemon should NOT retry the coordinator for this goal.

### Phase 2 - Coordinator Action Validation (No Prose Promises)

**Problem:** The coordinator writes execution intent in abandon reason strings but doesn't emit
the corresponding actions.

**Fix:** Two-part - validate action payloads and strip execution language from reason fields.

**2a. Action payload post-validation**

After parsing the coordinator's JSON action array, scan for `override_work` actions with
`target_status: "Abandoned"` and check whether the same payload contains corresponding
`create_work` actions when the reason mentions creating replacements.

```rust
fn validate_action_coherence(actions: &[AgentAction], prefix: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let abandon_actions: Vec<_> = actions.iter()
        .filter(|a| matches!(a, AgentAction::OverrideWork { target_status, .. }
            if target_status == "Abandoned"))
        .collect();

    let has_create = actions.iter().any(|a| matches!(a, AgentAction::CreateWork { .. }));

    for abandon in &abandon_actions {
        if let AgentAction::OverrideWork { reason, work_id, .. } = abandon {
            let lower = reason.to_lowercase();
            let mentions_create = lower.contains("creating")
                || lower.contains("create_work")
                || lower.contains("replacement")
                || lower.contains("replacing")
                || lower.contains("fix work");
            if mentions_create && !has_create {
                let msg = format!(
                    "{} override_work on {} mentions creating replacements \
                     but no create_work action in payload. Stripping promise from reason.",
                    prefix, work_id,
                );
                tracing::warn!("{}", msg);
                warnings.push(msg);
            }
        }
    }
    warnings
}
```

**Call site:** In `src/agents/coordinator/run.rs`, after action parsing (line 283) and before the
action execution loop (line 313). The warnings are logged but do not block execution:

```rust
let coherence_warnings = validate_action_coherence(&actions, &prefix);
for warning in &coherence_warnings {
    self.ctx.warn(warning);
}
```

**2b. Reason field sanitization**

When an `override_work` reason mentions creation but no `create_work` exists in the payload,
append a warning to the reason: "(NOTE: coordinator mentioned creating replacements but did not
emit create_work actions)". This surfaces the gap in the Learning/logs without blocking the
abandon.

**2c. Coordinator prompt reinforcement**

Add to coordinator.pmt in the Rules section:

```
- When abandoning a Work and creating a replacement, the `override_work` and `create_work`
  actions MUST appear in the SAME JSON array. Do NOT describe intent in the reason field
  without emitting the corresponding action. The system validates coherence: if your
  override_work reason mentions "creating" or "replacing" but no create_work is present,
  the system logs a warning and the replacement will NOT be created.
```

### Phase 3 - Reviewer Spec-Level AC Verification

**Problem:** The reviewer verifies tests pass (exit 0) but not whether they test the right things
relative to the spec contract.

**Fix:** Include ancestor spec AC in the reviewer's context so it can cross-check.

**3a. Context builder: inject spec AC into reviewer context**

In `src/agents/context.rs`, the `build()` method for reviewer role should include the spec-level
acceptance criteria alongside the work-level AC. Currently, parent Plan/Spec/Phase are provided as
markdown file links (lines 628-656). For the reviewer, expand the spec AC inline. Insert this
block after the Parent Context section (line 656) and before the Sibling Works section (line 660):

```rust
// In the reviewer context assembly (build() method), after the Parent Context section:
if self.role == Role::Reviewer {
    if let Some(ref spec_id) = self.spec_id {
        if let Ok(specs) = self.stores.read_specs() {
            if let Some(spec) = specs.get(spec_id.as_str()) {
                if !spec.acceptance_criteria.is_empty() {
                    msg.push_str(&format!(
                        "## Spec-Level Contract ({})\n\n\
                         The following acceptance criteria define the spec's contract. \
                         Verify that the implementation and tests are consistent with these:\n\n{}\n\n",
                        spec.title,
                        spec.acceptance_criteria.0.iter()
                            .map(|ac| format!("- {}", ac))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ));
                }
            }
        }
    }
}
```

**3b. Reviewer prompt: add spec-contract criterion**

Add a new blocking criterion to reviewer.pmt after criterion 4 (Scope):

```
5. **Spec Contract Alignment** - If a "Spec-Level Contract" section is provided above,
   verify that the implementation's data structures, field names, and API shapes match the
   spec's acceptance criteria. If the code uses field names or structures NOT defined in
   the spec contract (e.g., spec says `tags` but code uses `notes`), that is an `error`.
   This catches implementations that are internally consistent but diverge from the
   upstream contract.
```

Renumber existing criterion 5 (Cross-Module Calls) to 6, and subsequent criteria accordingly.

**Why this fixes the problem:** In the E2E run, the reviewer saw `test_database.py` testing
`notes`/`favorite` and `database.py` using `notes`/`favorite` - internally consistent. With spec
AC in context saying `create_bookmark() returns {id, title, url, tags}`, the reviewer can detect
that `notes` != `tags` and flag it as an error.

### Phase 4 - Implementer Field-Name Grounding

**Problem:** The implementer hallucinated `notes`/`favorite` instead of `tags` despite the work AC
specifying `tags`.

**Fix:** Make the work AC more prominent in the implementer's context and add a field-name
grounding instruction.

**4a. Implementer prompt: field-name grounding rule**

Add to implementer.pmt after the Scope Enforcement section:

```
## Field-Name Grounding

When implementing data structures, database schemas, or API models:
- Use the EXACT field names from your Work's acceptance criteria
- If the AC says "returns {id, title, url, tags}", your code MUST use those exact names
- Do NOT invent new field names (e.g., "notes" instead of "tags", "favorite" instead of
  "bookmarked") even if they seem more descriptive
- If the AC is ambiguous about a field name, create a Learning and need_help
```

**4b. Context builder: repeat raw AC in footer**

In the implementer's footer message (context.rs line 828-834), append the work's acceptance
criteria as a pre-commit checklist. No field extraction - just repeat the raw AC strings:

```rust
// In build(), when constructing the footer for Implementer role:
if self.role == Role::Implementer && !self.work_acceptance_criteria.is_empty() {
    msg.push_str("\n## Pre-Commit Checklist\n\n");
    msg.push_str("Verify your code satisfies each criterion before committing:\n");
    for ac in &self.work_acceptance_criteria {
        msg.push_str(&format!("- [ ] {}\n", ac));
    }
    msg.push('\n');
}
```

The implementer sees the AC twice: once in "Your Assignment" (top of context), once in the footer
(immediately before shipping). This doubles the salience without requiring field-name extraction.

**Why not code enforcement:** The field names in AC are free-text strings. Extracting them
programmatically would require NLP parsing. The reviewer with spec AC (Phase 3) catches the
failure if the prompt reinforcement doesn't prevent it.

## Alternatives Considered

### Alternative 1: Let the coordinator LLM make the quality decision in GoalComplete
- **Description:** Make the LLM actually run in GoalComplete state by not returning early from
  `apply_fsm_transition`. The LLM would then follow the quality gate prompt instruction.
- **Pros:** The coordinator can provide nuanced reasoning about why works were abandoned
- **Cons:** The v1 prompt instruction was already in place and the LLM ignored it. Even if the
  LLM did run, the GoalComplete footer (line 952) says "Respond with done" - contradicting the
  quality gate. LLM-driven safety gates are unreliable for "stop vs continue" decisions.
- **Why not chosen:** Code enforcement is deterministic. The LLM never needs to make this
  decision because the math is simple: count abandoned, divide by terminal, compare to threshold.

### Alternative 2: Parse coordinator reason strings and auto-create repair works
- **Description:** When the coordinator's override_work reason mentions "creating replacement",
  automatically generate the create_work action
- **Pros:** Recovers from the hallucinated-action failure mode automatically
- **Cons:** The system would be guessing at the repair work's AC, files, and dependencies.
  Auto-generating these from free text is fragile and could create worse problems than the
  missing repair.
- **Why not chosen:** Warning + logging is safer. The coordinator should learn to emit complete
  action arrays. If it consistently fails, the code gate (Phase 1) catches the cascading
  abandonments at GoalComplete.

### Alternative 3: Require the reviewer to read all parent docs
- **Description:** Expand the reviewer's context to include full Plan/Spec/Phase documents
- **Pros:** Maximum context for review decisions
- **Cons:** Massive token cost. Most reviews don't need plan-level context. The reviewer already
  gets the work AC, which should be sufficient for most cases.
- **Why not chosen:** Injecting just the spec AC (a few bullet points) gives the reviewer the
  contract without the token cost of full documents.

### Alternative 4: Post-implementation field-name validator
- **Description:** After the implementer commits, run a regex-based check that field names in
  the code match field names mentioned in the AC
- **Pros:** Deterministic catch of field-name drift
- **Cons:** AC text is free-form - extracting field names reliably requires NLP. False positives
  would block valid implementations. The reviewer already exists as the quality gate.
- **Why not chosen:** Prompt-level grounding (Phase 4) is lower risk. The reviewer with spec AC
  (Phase 3) provides the safety net. Code-level field extraction is a future option if both fail.

## Technical Considerations

### Dependencies

- Phase 1: `src/agents/coordinator.rs` (`apply_fsm_transition`), `src/agents/generation.rs`
  (new `goal_abandon_ratio_terminal`, existing `goal_abandon_ratio`), `prompts/coordinator.pmt`
  (remove dead GoalComplete instruction)
- Phase 2: `src/agents/coordinator.rs` (action processing), `prompts/coordinator.pmt`
- Phase 3: `src/agents/context.rs` (reviewer context), `prompts/reviewer.pmt`
- Phase 4: `prompts/implementer.pmt`, `src/agents/context.rs` (footer)

### Performance

- Phase 1: One `goal_abandon_ratio_terminal()` call at GoalComplete. O(W) where W = total works. Negligible.
- Phase 3: One additional spec read from in-memory store. Negligible.

### Security

No external inputs involved. No security implications.

### Testing Strategy

- **Phase 1:** Unit test: create stores with >40% terminal-abandoned ratio, verify
  `apply_fsm_transition(GoalComplete)` returns `IterationOutcome::NeedHelp`. Unit test: <=40%
  returns `IterationOutcome::Done`. Unit test: `goal_abandon_ratio_terminal` excludes non-terminal
  works from denominator. E2E: re-run python-api, verify the run exits with need_help error
  instead of declaring success.
- **Phase 2:** Unit test: `validate_action_coherence` with abandon+create pair (no warnings),
  abandon-only with "creating" in reason (warning emitted). E2E: verify coordinator logs show
  coherence warnings when promises aren't backed by actions.
- **Phase 3:** E2E: re-run python-api, verify reviewer catches `notes` vs `tags` mismatch in the
  database unit test work, preventing the false-positive approval.
- **Phase 4:** E2E: re-run python-api, verify implementer uses `tags` field name instead of
  hallucinating `notes`.

### Rollout Plan

Ship in priority order (most impactful first):

1. **Phase 1 - Code-enforced quality gate**: Highest impact. Deterministic. Prevents false
   GoalComplete regardless of LLM behavior.
2. **Phase 3 - Reviewer spec AC**: Catches the deepest failure (tests validating wrong schema).
   Moderate code change in context builder.
3. **Phase 4 - Implementer field grounding**: Prompt-only change. Low risk, addresses root cause.
4. **Phase 2 - Action coherence validation**: Warning-only. Useful for debugging but does not
   prevent the failure - Phase 1's code gate catches the downstream effect.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Code gate overrides valid GoalComplete (false positive on ratio) | Low | Med | The 40% threshold was validated in v1 design. Config-tunable via `max-abandon-ratio`. |
| Terminal-only denominator changes threshold behavior vs v1 | Low | Low | Terminal-only is strictly more aggressive (5/12 > 5/13). Old tests for `goal_abandon_ratio` remain; new tests cover `goal_abandon_ratio_terminal`. |
| Spec AC in reviewer context increases token usage beyond budget | Low | Low | Spec AC is typically 3-10 bullet points. Token impact is ~200 tokens per review. Well within budget. |
| Action coherence validator creates noise on intentional abandons without replacement | Med | Low | Only triggers when reason text mentions "creating"/"replacing". Intentional abandons with clear reasons ("dependency chain broken") won't trigger. |
| Implementer field-grounding rule is too rigid for creative implementations | Low | Low | Rule says "exact field names from AC" - if AC is vague, implementer can need_help. The rule constrains naming, not architecture. |
| Coordinator adapts to code gate by emitting need_help prematurely | Low | Med | need_help is the correct escalation path. Premature escalation is preferable to false completion. |
| Quality gate not applied to a new GoalComplete path added in future code | Med | High | Extract gate into a `check_abandon_gate()` helper. Grep for `GoalComplete` in coordinator.rs during code review to verify all paths are covered. |

## Open Questions

- [ ] Should the code gate in Phase 1 create a Learning record (visible in future coordinator
      runs if the goal is retried) or just log via tracing::warn? NeedHelp already surfaces to
      the TUI/E2E runner, but a Learning provides richer context for debugging.
- [ ] Phase 1: Should `IterationOutcome::NeedHelp` from the quality gate be distinguishable from
      LLM-emitted need_help? The run loop treats them identically (exit with error), but the
      E2E runner might want to know the gate fired vs the LLM choosing to escalate.
- [ ] Phase 3: Should spec AC injection apply to all reviewer runs or only when the work involves
      test files? Test-only scoping reduces noise but risks missing schema drift in non-test code.
- [ ] Phase 2: Should the action coherence validator reject the action payload entirely (forcing
      re-iteration) or just warn and proceed? Rejection is safer but adds latency.

## References

- `docs/design/2026-04-08-e2e-post-mortem-hardening.md` - v1 hardening design doc (Implemented)
- `src/agents/coordinator.rs:735-743` - ActivatePhase fallback to GoalComplete (primary path, no gate)
- `src/agents/coordinator.rs:750-767` - `apply_fsm_transition` explicit GoalComplete branch (returns early)
- `src/agents/coordinator.rs:952-958` - `build_fsm_footer` GoalComplete (dead code - LLM never runs)
- `src/agents/coordinator/run.rs:43-46` - `is_terminal()` check exits loop before LLM runs
- `src/agents/coordinator/run.rs:159-164` - FSM transition fires before LLM in `run_iteration`
- `src/agents/generation.rs:191` - `goal_abandon_ratio()` utility (exists, unused at decision point)
- `src/agents/context.rs:248` - `spec_id: Option<String>` field for spec lookup
- `src/agents/context.rs:628-656` - Parent context links (currently markdown links only)
- `prompts/coordinator.pmt:21-26` - GoalComplete quality gate instruction (dead - LLM never runs)
- `prompts/reviewer.pmt:8` - Acceptance criteria criterion
- `prompts/implementer.pmt:56-59` - Scope enforcement
- `prompts/decompose/work.pmt:65-72` - AC grounding rule
