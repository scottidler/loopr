# Design Document: Reviewer Acceptance Criteria Gate

**Author:** Scott Idler + Claude
**Date:** 2026-03-31
**Status:** Implemented
**Review Passes Completed:** 3/5

## Summary

The reviewer agent rejects bundles for subjective quality issues (naming
conventions, missing input validation, style) that aren't in the Work's
acceptance criteria, causing rejection loops that waste tokens and prevent
goal completion. Fix by making acceptance criteria the primary gate and
demoting subjective quality to non-blocking observations.

## Problem Statement

### Background

The Python todo E2E run on 2026-03-31 demonstrated that the FSM recovery
architecture works perfectly - rejected bundles trigger override_work, the
worker pool re-assigns, and the implementer retries. But the reviewer
rejected 3 consecutive bundles for increasingly pedantic reasons:

1. "filter='pending' vs filter='active' naming inconsistency"
2. "parameter name filter_by vs filter may break sibling Work"
3. "missing non-empty title validation"

None of these were in the Work's acceptance criteria. The core CRUD
functionality worked and tests passed in all three bundles.

### Problem

The reviewer prompt (`prompts/reviewer.pmt`) treats subjective quality as
equal to objective correctness:

- **Quality** criterion: "clean, idiomatic, maintainable" (subjective)
- **Tests** criterion: "every public function should have at least one test"
  (may exceed acceptance criteria)
- **request_changes** threshold: "any error-severity issue OR 3+ warnings"
  (style warnings count)

This means the reviewer can block a bundle that satisfies all acceptance
criteria by finding 3 style warnings. In an autonomous pipeline with no human
to override, this creates an unbounded rejection loop.

This is a systemic issue, not an E2E issue. When the coordinator generates
its own plans (no pre-written `--plan`), there is no way to inject a review
policy. The reviewer's default behavior must be acceptance-criteria-first.

### Goals

- Acceptance criteria are the primary gate for approval
- Subjective quality issues are non-blocking observations
- Security and safety issues remain blocking
- Reviewer still provides useful feedback (not rubber-stamping)

### Non-Goals

- Removing quality feedback entirely (still valuable as info)
- Changing the reviewer's code analysis capability
- Adding a rejection cycle cap to Work items (separate concern)
- Modifying the coordinator's plan generation

## Proposed Solution

### Phase 1: Update Reviewer Prompt

**File:** `prompts/reviewer.pmt`

Replace the current Review Criteria and Verdict Thresholds sections:

```
## Review Criteria

Your primary job is evaluating the code against the Work's acceptance_criteria.

### Blocking (can cause rejection)
1. **Acceptance Criteria** - Does the code satisfy EVERY criterion in the Work?
   Failure is an `error`.
2. **Safety & Security** - OWASP vulnerabilities, injection risks, critical
   concurrency flaws (deadlocks, data races). Failure is an `error`.
3. **Build & Tests** - Does the code compile/run and do the existing tests pass?
   Failure is an `error`.

### Non-Blocking (observations only)
4. **Quality & Style** - Idiomatic code, naming conventions, parameter style.
   These are `info` or `warning` only. They CANNOT block approval.
5. **Extra Test Coverage** - Tests beyond what acceptance criteria require.
   Missing nice-to-have tests are `info` only.
6. **Defensive Coding** - Input validation, type hints, edge-case handling
   beyond what was requested. These are `info` only.

## Verdict Thresholds

- **approve**: All acceptance_criteria are met AND no security/safety errors.
  You MUST approve even if there are warnings or info observations.
- **request_changes**: One or more acceptance_criteria are NOT met, OR there
  is a security/safety error.
- **reject**: Fundamental design flaw requiring a completely new approach, OR
  critical security vulnerability.
```

### Phase 2: Remove E2E Plan Workaround

**File:** `bin/e2e-targets/python-todo.sh`

Remove the `REVIEW POLICY` block from the plan. With the systemic fix in
the reviewer prompt, the E2E plan doesn't need to constrain the reviewer.

## Alternatives Considered

### Alternative 1: Plan-Level Review Policy

- **Description:** Add a `REVIEW POLICY` to each plan that constrains the
  reviewer's scope.
- **Pros:** Targeted, doesn't change default reviewer behavior.
- **Cons:** Only works for pre-written plans. Coordinator-generated plans
  won't include it. Every plan needs the same boilerplate.
- **Why not chosen:** Band-aid that doesn't fix the systemic issue.

### Alternative 2: Rejection Cycle Cap on Work Items

- **Description:** Track `rejection_count` on Work items, abandon after N
  rejections.
- **Pros:** Hard limit prevents unbounded loops.
- **Cons:** Treats the symptom (loops) not the cause (bad rejections).
  A working implementation gets abandoned because the reviewer is too strict.
- **Why not chosen:** Better to fix the reviewer's judgment than to cap its
  attempts. Can be added later as defense-in-depth.

### Alternative 3: Coordinator Override of Reviewer

- **Description:** Let the coordinator override reviewer rejections if it
  believes acceptance criteria are met.
- **Pros:** Keeps reviewer strict, adds a check.
- **Cons:** Adds complexity. Coordinator would need to re-review code.
  Two LLM calls instead of one, and they might disagree indefinitely.
- **Why not chosen:** Simpler to fix the reviewer's criteria hierarchy.

## Technical Considerations

### Dependencies

No code changes. Only prompt file modification.

### Testing Strategy

- **E2E:** Re-run `bin/e2e.sh --target python-todo` and verify the reviewer
  approves bundles that satisfy acceptance criteria, even with style nits.
- **Regression:** Re-run `bin/e2e.sh` (rust-version default) to verify the
  reviewer still catches genuine issues.
- **Manual inspection:** Review the reviewer's output JSON to confirm style
  issues appear as `info`/`warning` not `error`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Reviewer becomes too lenient, approves buggy code | Low | Med | Acceptance criteria and security checks are still blocking; only style is demoted |
| LLM ignores prompt hierarchy and still rejects on style | Med | Low | The prompt is explicit about MUST approve; if still happens, add few-shot examples |
| Acceptance criteria are too vague to gate on | Med | Med | Separate concern - coordinator needs to generate concrete criteria. Not this doc's scope |

## Open Questions

- [ ] Should the reviewer include a `criteria_checklist` in its output JSON
      showing pass/fail per acceptance criterion? This would make the gate
      mechanically verifiable rather than relying on LLM judgment.

## References

- Current reviewer prompt: `prompts/reviewer.pmt`
- E2E session showing rejection loop: `~/.local/share/loopr/sessions/20260331T202103/`
- Rejection recovery design: `docs/design/2026-03-31-rejection-recovery-circuit-breaker.md`
