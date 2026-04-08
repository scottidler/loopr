# Design Document: E2E Decomposition and Noop Bundle Fixes

**Author:** Scott Idler + Claude
**Date:** 2026-04-08
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The python-api E2E run exposed four issues that together prevented the pipeline from completing. Two are prompt/context assembly bugs causing decomposition validation floods. One is a state transition gap causing a noop bundle death loop. The fourth is a symptom of the third. This document addresses all four as a single coordinated fix.

## Problem Statement

### Background

The Loopr orchestrator decomposes Plans into Specs, Phases, and Work items via LLM calls, then validates each generated document against its template. After decomposition, Workers claim Ready Work items, Implementers write code in worktrees, and the Integrator merges bundles back to main. The recent python-api E2E run failed on both ends of this pipeline.

### Problem

**Issue 1: Decompose prompt is blind to the template.**
`build_decompose_prompt` (src/decomposer.rs:79-88) sends the LLM only the .pmt instruction file and the parent document. The actual template content (from docs/templates/) is never included. The LLM must guess section names. Meanwhile, `build_validate_prompt` (src/decomposer.rs:91-103) does include the template via `include_str!`. This asymmetry guarantees validation failures: the LLM generates documents it cannot see the spec for, then a validator with the full spec grades them.

**Issue 2: The .pmt instruction files are vague about section names.**
- `spec.pmt` line 26: "follow the Spec template structure (Overview, Data Flow, Module Structure, etc.)" - "etc." is what breaks it
- `phase.pmt` line 26: lists top-level sections but never mentions the `### This phase validates with:` / `### This phase does NOT validate with:` subsection structure
- `work.pmt` line 27: lists required sections but omits `### Phase Validation Scope`, `### Design Decisions`, `### Open Questions`; says AC must be asserts but doesn't convey the format strongly enough

Even with the template injected (Issue 1 fix), vague .pmt instructions dilute the template's authority by implying a looser structure is acceptable.

**Issue 3: Noop bundle death loop.**
After `database.py` was written and merged via the first real bundle, `wk-2qge6` was never transitioned to Done. Workers kept re-claiming it. Implementers saw the file already existed and proposed noop bundles. The integrator correctly skipped noop merges. The work reset to Ready. Repeat until Abandoned. All 25 other Ready items sat untouched for the entire run.

The Work FSM path for a successful bundle is: Ready -> InProgress -> InReview -> Integrated -> Done. The integrator handles InReview -> Integrated (src/agents/integrator.rs:745-779), but only when `w.status() == WorkStatus::InReview`. The executor's `determine_work_handback` (src/agents/executor/util.rs:63-89) transitions InProgress -> InReview when an active bundle exists. For the noop death loop to occur, either: (a) the handback didn't fire, (b) the Work wasn't InReview when the integrator checked, or (c) the noop bundle was rejected before integration, resetting the Work to Ready via `reset_work_after_bundle_rejection`. Given that the 2026-04-06 design doc identifies the Reviewer rejecting noop bundles due to missing file contents as the primary failure mode, (c) is the most likely root cause in the observed run.

**Issue 4: Workers not spreading across Ready items.**
With 2 workers and 25+ Ready items, both workers cycled on the same broken Work item. This is a symptom of Issue 3 - the work queue picks the highest-priority Ready item, and when the same item keeps returning to Ready, it keeps winning.

### Goals

- Inject the template into decompose prompts so the LLM can see what the validator will grade against
- Simplify .pmt files to defer to the injected template rather than duplicating (and contradicting) section lists
- Ensure noop bundles flow through the full FSM path to Done
- Prevent a single cycling Work item from starving the rest of the Ready pool

### Non-Goals

- Rewriting the decomposer's retry/validation loop (it works correctly once prompts are fixed)
- Changing the Work FSM states or transitions
- Changing how normal (non-noop) bundles work
- Fixing the noop reviewer context bug (covered by 2026-04-06 design doc, complementary to this doc)

## Proposed Solution

### Overview

Four changes, one per issue:

1. **Template injection in `build_decompose_prompt`** - include the template content alongside the .pmt instructions
2. **Simplify .pmt files** - remove duplicated section lists; point at the injected template
3. **Work queue retry penalty** - penalize Work items that cycle back to Ready, preventing a single broken item from starving the pool
4. **Workers not spreading** - resolved by Fix 3; no additional changes needed

### Fix 1: Template Injection in `build_decompose_prompt`

**File: `src/decomposer.rs`, lines 79-88**

The current code only sends the .pmt content and the parent document. Add the template content via `include_str!`, mirroring what `build_validate_prompt` already does.

```rust
fn build_decompose_prompt(target_kind: DocKind, parent_content: &str) -> Result<String> {
    let prompts = crate::prompts::store();
    let (instructions, template_text) = match target_kind {
        DocKind::Spec => (&prompts.decompose_spec, include_str!("../docs/templates/spec.md")),
        DocKind::Phase => (&prompts.decompose_phase, include_str!("../docs/templates/phase.md")),
        DocKind::Work => (&prompts.decompose_work, include_str!("../docs/templates/work.md")),
        DocKind::Plan => bail!("cannot decompose into Plan"),
    };
    Ok(format!(
        "{}\n\n## Template\n\n{}\n\n## Parent Document\n\n{}",
        instructions, template_text, parent_content
    ))
}
```

**Why this works:** The validator and the generator now see the same template. The LLM no longer has to guess section names.

### Fix 2: Simplify .pmt Instruction Files

The .pmt files currently duplicate section lists from the templates, poorly. With the template now injected directly into the prompt, the .pmt files should defer to the template rather than re-listing sections.

**File: `prompts/decompose/spec.pmt`**

Replace rule 1:
```
1. Each Spec must follow the Spec template structure (Overview, Data Flow, Module Structure, etc.)
```
With:
```
1. Each Spec must follow the template provided in the "## Template" section below EXACTLY - use the same section headings, include all required sections, and follow the structural conventions shown
```

**File: `prompts/decompose/phase.pmt`**

Replace rule 1:
```
1. Each Phase must follow the Phase template structure (Parent, Deliverables, Validation, Dependencies, Work Items)
```
With:
```
1. Each Phase must follow the template provided in the "## Template" section below EXACTLY - use the same section headings and subsection structure (including ### sub-headings within sections)
```

**File: `prompts/decompose/work.pmt`**

Replace rule 1:
```
1. Each Work must follow the Work template structure (Parent, Description, Inputs, Outputs, Constraints, Implementation Notes, Acceptance Criteria, Dependencies)
```
With:
```
1. Each Work must follow the template provided in the "## Template" section below EXACTLY - use the same section headings, include all conditional sections that are relevant, and write Acceptance Criteria as assert statements (never prose)
```

### Fix 3: Work Queue Retry Penalty (Death Loop Safety Net)

The noop death loop has a root cause and a starvation symptom:

**Root cause (fixed by 2026-04-06 doc, not yet implemented):** The ContextBuilder fails to provide file contents for noop bundles, so the Reviewer always rejects them. `reset_work_after_bundle_rejection` (src/agents/integrator.rs:1032) uses an override transition to put Work back to Ready. Worker reclaims, implementer re-proposes noop, cycle repeats.

**Starvation symptom (fixed here):** While the broken Work cycles, it keeps winning priority in the work queue because it has the same score as fresh items. Both workers pile onto the same item, leaving 25+ other Ready items untouched.

The integrator's noop path is actually correct once the reviewer approves: noop bundles stay in `valid_bundle_ids`, the C1 block (src/agents/integrator.rs:733-779) transitions their parent Work InReview -> Integrated, and `sweep_integrated_to_done` completes the cycle to Done. The gap is upstream - rejection before integration.

**Both a retry penalty AND a hard limit are required.** The penalty ensures other Ready items get served while a cycling Work item is within its attempt budget. The hard limit provides the terminal backstop - without it, the cycling item would eventually surface to the top of an empty queue and resume its infinite cycle.

**File: `src/daemon/handlers/work.rs`** - add `MAX_WORK_ATTEMPTS` constant:

```rust
/// Maximum number of times a Work item can be reset to Ready before being Abandoned.
/// Prevents infinite noop death loops when the root cause is unresolvable by workers.
const MAX_WORK_ATTEMPTS: u32 = 5;
```

**File: `src/domain/work.rs`**

Add `attempt_count: u32` field (serde default 0). Increment it whenever the Work transitions back to Ready from a non-Draft state.

**File: `src/daemon/work_queue.rs`, function `compute_priority`**

Add a retry penalty. Track `attempt_count` on Work (number of times it has been claimed and returned to Ready). Penalize Work that keeps cycling:

```rust
// Penalize Works that have been attempted and reset multiple times.
// Each failed attempt reduces priority, preventing a single broken
// Work item from starving the rest of the pool.
score -= (work.attempt_count.min(5) as i64) * 50;
```

**File: `src/daemon/handlers/work.rs` (`handle_work_transition`, line 418)**

The handler calls `wi.force_status(target_status)` at line 418 after FSM validation. Insert the increment and hard-limit check immediately before that line:

```rust
// Increment attempt_count when Work is being reset to Ready from a non-Draft state.
// If the item exceeds MAX_WORK_ATTEMPTS, override target to Abandoned - this is the
// terminal backstop that prevents an infinite noop death loop.
let effective_status = if target_status == WorkStatus::Ready && from != WorkStatus::Draft {
    wi.attempt_count += 1;
    if wi.attempt_count >= MAX_WORK_ATTEMPTS {
        WorkStatus::Abandoned
    } else {
        target_status
    }
} else {
    target_status
};
wi.force_status(effective_status);
```

This fires on both normal transitions (Blocked -> Ready) and overrides (InProgress -> Ready, InReview -> Ready). When `attempt_count` reaches `MAX_WORK_ATTEMPTS`, the item transitions to Abandoned regardless of the caller's requested target.

### Fix 4: Workers Not Spreading (Resolved by Fix 3)

Fix 3's retry penalty directly addresses this. A Work item that returns to Ready after a failed attempt gets a -50 score penalty per attempt. After just one failed cycle, fresh Ready items (score 200) will be prioritized over the cycling item (score 150). After two cycles, the gap widens to 100 points.

No additional changes needed beyond Fix 3.

## Alternatives Considered

### Alternative 1: Hard-block Work after N failed attempts (without priority penalty)

- **Description:** After N attempts, permanently mark the Work as Abandoned. No priority penalty - just a binary cut.
- **Pros:** Simpler, no scoring changes.
- **Cons:** Other Ready items are not prioritized over the cycling item within its attempt budget. Both workers pile onto the cycling item until it hits the limit, leaving the rest of the pool untouched until then.
- **Why not chosen (alone):** The hard limit (`MAX_WORK_ATTEMPTS`) is necessary and included in Fix 3. But without the priority penalty, starvation still occurs within the attempt budget window. Fix 3 combines both: penalty spreads work during the budget window, hard limit terminates the cycle after the budget is exhausted.

### Alternative 2: Duplicate template content into .pmt files instead of injecting

- **Description:** Copy the full template text into each .pmt file so it's visible to the LLM without code changes.
- **Pros:** No code change to `build_decompose_prompt`.
- **Cons:** Duplicates the template in two places (docs/templates/ and prompts/decompose/). Templates evolve, and the copies will drift. The validator would still use the canonical template while the generator uses the copy.
- **Why not chosen:** Single source of truth. The code change is trivial and eliminates drift risk.

### Alternative 3: Remove the validator entirely (trust the template injection)

- **Description:** If the LLM sees the template, it will follow it. Remove the validation step.
- **Pros:** Eliminates the validation retry loop entirely. Faster decomposition.
- **Cons:** LLMs still drift, especially on conditional sections and subsection structure. The validator catches real errors.
- **Why not chosen:** Defense in depth. The validator is cheap (one LLM call) and catches genuine structural issues. The fix is to make the generator and validator see the same spec, not to remove verification.

## Technical Considerations

### Dependencies

- Fix 1: No new dependencies. Uses existing `include_str!` pattern.
- Fix 2: No dependencies. Prompt text changes only.
- Fix 3: Adds `attempt_count: u32` field to Work domain model with `#[serde(default)]` for backward compatibility with existing JSONL records. Adds `MAX_WORK_ATTEMPTS: u32 = 5` constant to `src/daemon/handlers/work.rs`. When a Work would be reset to Ready but has reached the limit, it is transitioned to Abandoned instead.
- Fix 4: No additional dependencies.

### Performance

- Fix 1 slightly increases prompt token count (template text added to decompose calls). Offset by fewer validation retries.
- Fix 3 adds one integer comparison to the priority scoring hot path. Negligible.

### Backward Compatibility

- `attempt_count: u32` with `#[serde(default)]` ensures existing Work records deserialize with 0 attempts. No migration needed.
- .pmt prompt changes are backward compatible - they change LLM instructions, not IPC protocols.

### Testing Strategy

1. **Fix 1 unit test:** Call `build_decompose_prompt(DocKind::Spec, "parent")` and assert the output contains `## Template` and the spec template's `## Overview` heading.
2. **Fix 1 unit test:** Verify `build_decompose_prompt` and `build_validate_prompt` include the same template text for each DocKind.
3. **Fix 3 unit test:** Create a Work with `attempt_count = 3`, verify `compute_priority` returns a lower score than a Work with `attempt_count = 0`.
4. **Fix 3 unit test:** Transition a Work from InProgress -> Ready, verify `attempt_count` increments.
5. **Fix 3 unit test:** Two Ready works, one with `attempt_count = 2`, one fresh. Verify `next_assignable_work` returns the fresh one.
6. **Fix 3 unit test:** Work with `attempt_count = MAX_WORK_ATTEMPTS - 1` being reset to Ready should transition to `WorkStatus::Abandoned` instead, and `attempt_count` should equal `MAX_WORK_ATTEMPTS`.
7. **E2E validation:** Re-run the python-api E2E target after all fixes. Decomposition should complete with zero validation warnings. The first Work should complete and reach Done. Remaining Works should be claimed by workers in round-robin fashion.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Template injection increases token count beyond model limit | Low | Medium | Templates are 50-120 lines each. Combined with .pmt and parent doc, well within 200k context. Monitor token usage in first E2E run. |
| Retry penalty causes legitimate retries to be deprioritized | Low | Low | Penalty is -50 per attempt, not a hard block. After the root cause (reviewer context) is fixed, retries succeed and the Work completes. Score recovers because fresh Ready items are picked first, giving the system time to process the noop correctly. |
| .pmt simplification removes important guidance the LLM relied on | Low | Medium | The .pmt files still contain the role description, input/output format, and rules. Only the section list is replaced with a pointer to the template. The template itself is more detailed than the .pmt ever was. |

## Edge Cases

- **Legitimate retries (merge conflicts, transient LLM failures):** The penalty is -50/attempt. A Work with 2 failed attempts (score ~100) is still claimable - just lower priority than fresh items (score ~200). `MAX_WORK_ATTEMPTS = 5` is the terminal backstop: a Work item that genuinely cannot complete (e.g., permanent noop loop before the reviewer context fix lands) will be Abandoned after 5 attempts, freeing the queue permanently.
- **`attempt_count` only increments on Reset-to-Ready:** The work queue's `next_assignable_work` uses `force_status(InProgress)` directly (src/daemon/work_queue.rs:88), bypassing `handle_work_transition`. This means claiming a Work doesn't increment `attempt_count` - only being sent back to Ready does. Correct behavior.
- **Work with both contention penalty and attempt penalty:** Score can go negative (e.g., contested + 3 attempts = -150 + 80 = score < 0). This is fine - the item just waits until higher-priority items are claimed. It's never permanently blocked.

## Open Questions

- [ ] Should `attempt_count` be reset to 0 when a Work transitions to Done? (Probably not - it's a historical counter, not active state. But confirm.)

## Implementation Order

1. Fix 1 (template injection) + Fix 2 (.pmt simplification) - these are coupled and should land together
2. Fix 3 (retry penalty + attempt_count) - independent, can land separately
3. Re-run E2E to validate

Fixes 1+2 address the validation warning flood. Fix 3 addresses the death loop starvation symptom.

**Relationship to 2026-04-06 noop reviewer context fix (status: Draft, not yet implemented):** That fix addresses the root cause - the reviewer rejecting noop bundles because file contents are missing from context. This doc's Fix 3 provides the safety net: the `MAX_WORK_ATTEMPTS = 5` hard limit ensures that even without the root cause fix, a permanently cycling Work item terminates after 5 attempts instead of looping forever. Implementation order: this doc's Fix 3 is safe to land independently. The 2026-04-06 fix reduces how often the hard limit triggers, but Fix 3 is not blocked on it.

## References

- `src/decomposer.rs:79-103` - `build_decompose_prompt` and `build_validate_prompt`
- `prompts/decompose/spec.pmt`, `phase.pmt`, `work.pmt` - decomposition instruction files
- `docs/templates/spec.md`, `phase.md`, `work.md` - canonical templates
- `src/daemon/work_queue.rs:34-101` - work queue scoring and claiming
- `src/domain/work.rs` - Work domain model
- `src/agents/integrator.rs:537-779` - integrator merge and C1 Work transition logic
- `src/agents/executor/util.rs:56-89` - `determine_work_handback`
- `docs/design/2026-04-01-noop-bundle-pathway.md` - original noop design
- `docs/design/2026-04-06-noop-reviewer-context-fix.md` - complementary reviewer context fix
- `docs/design/2026-04-08-e2e-python-api-noop-commit-bug.md` - failure report documenting the death loop
