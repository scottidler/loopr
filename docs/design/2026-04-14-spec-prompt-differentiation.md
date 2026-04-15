# Design Document: Spec Prompt Differentiation

**Author:** Scott A. Idler
**Date:** 2026-04-14
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

The Spec template (`docs/templates/spec.md`) defines a clear Tech Spec structure -
data flow, module structure, failure modes, test inventory - but the prompts that
generate, validate, and evaluate Specs don't enforce that structure. Three `.pmt`
files need rewriting to close the gap between what a Spec *should* be and what the
LLM actually produces.

## Problem Statement

### Background

Loopr's document hierarchy is Plan -> Spec -> Phase -> Work. The Plan is the PRD
(what and why). The Spec is the Tech Spec (how). The templates draw this line
clearly with distinct section headers and different required content.

### Problem

The prompts that operate on Specs treat them as generic documents rather than
technical specifications:

1. **`decompose/spec.pmt`** tells the LLM to "follow the template" but provides no
   interpretive guidance about what makes a good Spec. It has 6 mechanical JSON-formatting
   rules and zero architectural reasoning rules. Compare to `decompose/work.pmt` which has
   a full "Parallelism" section, an "AC Grounding Rule", and explicit guidance about file
   ownership - all things that make Work items useful beyond template compliance.

2. **`validator-spec.pmt`** checks 4 generic criteria ("technical approach described",
   "testability addressed") that could apply to any document. It doesn't check whether
   the Spec template's required sections are substantive - a Spec that says "## Data Flow\n\nData flows through the system." would pass.

3. **`coverage-plan-specs.pmt`** checks requirement gaps and coherence but doesn't
   check architectural coverage - whether the Specs' module structures collectively
   cover the Plan's contract boundaries.

The result: Spec quality depends entirely on the LLM's inherent tendency to follow
templates faithfully. When it drifts, nothing catches it. Plans get interview
refinement; Specs get one shot.

### Goals

- Spec decomposition prompt produces Specs where every section is substantive and
  traceable to the parent Plan
- Spec validator catches hollow sections (template headings with no real content)
- Coverage evaluator checks architectural coverage, not just requirement mapping
- All changes are prompt-only (.pmt files) - no Rust code changes

### Non-Goals

- Adding fields to the Spec domain struct (future optimization, not needed now)
- Building a Spec refinement/interview loop (separate feature, larger scope)
- Changing the Spec template itself (the template is good; the prompts are the problem)
- Changing Plan prompts (Plan prompts are already appropriately specific)

## Proposed Solution

### Overview

Rewrite three `.pmt` files to make each prompt Spec-aware: the decomposer explains
what good Spec content looks like, the validator checks for it, and the coverage
evaluator verifies architectural completeness.

### File 1: `prompts/decompose/spec.pmt`

Current prompt is 36 lines: role statement, JSON format, 6 rules about formatting
and dependency chaining. No guidance about what the sections should contain.

Proposed additions after the existing rules. Note: the Rust code in
`decomposer.rs:409-411` appends `## Guidance`, `## Template`, and `## Parent Document`
sections after the prompt text, so these additions appear before those injected sections.

```
## Architectural Guidance

When writing each Spec's content, ensure:

### Data Flow
- Trace the Plan's contracts through the system end to end
- Show concrete data shapes at each boundary (what enters, what exits)
- Include both the happy path AND at least one error path
- If the Plan defines a data model with fields X, Y, Z, show where those fields
  are created, transformed, and consumed

### Module Structure
- Every file must trace to at least one Plan requirement
- State what each file exports and what it depends on
- If two Specs share a dependency, name it explicitly in both
- Do NOT list files that have no clear purpose in the Plan's requirements

### Interfaces
- Elaborate the Plan's contracts with module-level detail
- Function signatures must include parameter types and return types
- If the Plan defines an API endpoint, the Spec must show the handler signature,
  the request/response types, and which module owns it

### Failure Modes
- At least one failure mode per interface boundary
- Include the handling strategy and rationale, not just "returns error"
- Cover: malformed input, missing dependencies, concurrent access where applicable

### Testing (including ### Test Inventory and ### Not Tested subsections)
- Number every test in the Test Inventory subsection
- Each Plan AC that maps to this Spec must map to at least one test
- The Not Tested subsection must be present and state exclusions explicitly

### Traceability
- The Overview must name which Plan requirements this Spec covers
- If a requirement spans multiple Specs, each Spec states its portion

### When the Plan has minimal contracts
- If the Plan's Contracts section says "No contract changes" or references a shared
  spec, Data Flow and Interfaces should focus on how existing contracts are consumed,
  not on defining new ones. Module Structure and Testing sections still apply fully.
```

### File 2: `prompts/validator-spec.pmt`

Current prompt checks 4 generic criteria. Replace with Spec-specific checks:

```
## Evaluation Criteria

1. **Plan alignment** - The Spec's Overview names which Plan requirements it covers.
   The approach directly addresses those requirements and the Plan's acceptance criteria.

2. **Data Flow completeness** - The Data Flow section traces data end to end with
   concrete shapes at each boundary. Both happy path and at least one error path are
   present. Missing or single-sentence Data Flow sections are a blocking issue.

3. **Module Structure substantiveness** - The Module Structure section lists files with
   responsibility, dependencies, and exports. An empty table or a table with only
   filenames (no responsibility column) is a blocking issue.

4. **Interface elaboration** - The Interfaces section elaborates the Plan's contracts
   with module-level detail (function signatures, parameter types, return types).
   Interfaces that merely repeat the Plan's contract definitions without adding
   module-level specificity are insufficient.

5. **Failure Modes coverage** - The Failure Modes section has at least one entry per
   interface boundary defined in the Interfaces section. Entries must include handling
   strategy and rationale. An empty Failure Modes table is a blocking issue.

6. **Test Inventory traceability** - The Testing section includes a numbered test
   inventory. Every Plan AC traceable to this Spec should map to at least one test.
   The "Not Tested" subsection is present and states exclusions explicitly.

7. **Optional sections** - Performance, Security, Open Questions, Alternatives, Key
   Decisions, Glossary are conditional. Their absence must NOT lower the verdict. Only
   flag them if their content is present but incorrect or incomplete.
```

### File 3: `prompts/coverage-plan-specs.pmt`

Current prompt checks gaps, out-of-scope, and coherence. Add architectural checks:

```
## Task
Evaluate whether these Specs, taken together, fully address the Plan's acceptance
criteria, requirements, and contract boundaries.

Check for:
1. **Requirement gaps**: Are there acceptance criteria or requirements in the Plan
   that no Spec addresses?
2. **Contract coverage**: Does every data model entity and API endpoint defined in
   the Plan's Contracts section appear in at least one Spec's Module Structure or
   Interfaces section?
3. **Interface consistency**: Where two Specs share an interface boundary, do their
   descriptions of that boundary agree on types, parameters, and behavior?
4. **Out-of-scope**: Do any Specs include modules, interfaces, or data flows that
   go beyond the Plan's stated goals?
5. **Coherence**: Do the Specs work together without contradictions or overlapping
   file ownership?
```

### Implementation Plan

#### Phase 1: Rewrite decompose/spec.pmt
**Model:** opus
- Keep lines 1-36 (role, JSON format, rules 1-6) exactly as-is
- Append the `## Architectural Guidance` section with subsections for Data Flow,
  Module Structure, Interfaces, Failure Modes, Testing, and Traceability
- No Rust code changes - `build_decompose_prompt()` in `decomposer.rs` appends
  `## Guidance`, `## Template`, and `## Parent Document` after the prompt text

#### Phase 2: Rewrite validator-spec.pmt
**Model:** opus
- Keep the role statement (lines 1-2) and output format section (lines 15-18)
- Keep the `{markdown_content}`, `{parent_markdown_content}`, `{schema}` placeholders
- Replace the `## Evaluation Criteria` section with the 7 Spec-specific criteria.
  All criteria are blocking (pass/fail) consistent with the current validator behavior

#### Phase 3: Rewrite coverage-plan-specs.pmt
**Model:** sonnet
- Keep the `## Parent Plan` and `## Generated Specs` sections with their placeholders
- Replace the `## Task` section's 3-item checklist with the 5-item checklist adding
  contract coverage and interface consistency checks
- Keep the `{schema}` placeholder and JSON output instruction

#### Phase 4: Validate with E2E
**Model:** sonnet
- Build with `otto ci` to verify prompts compile (they're `include_str!`)
- Run the e2e skill against a target repo with a Full-mode Plan that defines contracts
- Inspect generated Specs in `docs/loopr/` for: non-empty Data Flow, populated Module
  Structure table, numbered Test Inventory, Failure Modes with rationale
- Manually feed a hollow Spec (headers only, no content) to the validator endpoint
  and confirm it rejects

## Alternatives Considered

### Alternative 1: Add metadata fields to Spec struct
- **Description:** Add `module_count`, `test_count`, `interface_count` to `Spec` in Rust,
  extracted during validation. Gives Coordinator quick signal about Spec quality.
- **Pros:** Programmatic quality signal without re-reading markdown
- **Cons:** Rust code changes (domain struct, serde, decomposer handler, indexed fields).
  The Coordinator already reads the full markdown for context assembly.
- **Why not chosen:** The Coordinator doesn't currently make decisions based on numeric
  quality metrics. If it starts needing that, this becomes worthwhile. Until then,
  the prompts are sufficient.

### Alternative 2: Build a Spec refinement/interview loop
- **Description:** Add interactive Spec refinement similar to the Plan interview pipeline.
  New Coordinator sub-state, new IPC messages, chat-like back-and-forth.
- **Pros:** Highest quality Specs; human-in-the-loop for architectural decisions
- **Cons:** Significant Rust work (Coordinator FSM, chat system, new prompts). Changes
  the execution model - currently Specs are generated and validated, not iterated.
- **Why not chosen:** The decompose -> validate -> coverage pipeline already has retry
  logic. Better prompts should produce good Specs on first attempt. If they don't, the
  retry catches it. A refinement loop is a separate feature for when we have evidence
  that retries aren't sufficient.

### Alternative 3: Change the Spec template
- **Description:** Add more prescriptive guidance directly in the template markdown.
- **Pros:** Single source of truth
- **Cons:** The template is already good - it defines the right sections. The problem
  is that the prompts don't guide the LLM toward filling those sections substantively.
  Making the template longer doesn't help if the decomposer prompt doesn't reference it.
- **Why not chosen:** Template defines structure. Prompts define quality expectations.
  These are different concerns.

## Technical Considerations

### Dependencies
None. All three files are standalone `.pmt` templates with placeholder variables
that are already handled by existing Rust code.

### Performance
No impact. The prompts are slightly longer but well within LLM context limits.
The decompose/spec prompt grows from ~36 lines to ~70 lines.

### Testing Strategy
- Unit: existing prompt tests in `src/prompts.rs` verify placeholder substitution.
  No new Rust tests needed since no Rust code changes.
- Integration: E2E run with a Full-mode Plan that defines contracts. Inspect generated
  Specs for substantive sections.
- Regression: E2E run with a Brief-mode Plan to confirm Spec generation is still skipped.

### Rollout Plan
Direct replacement of `.pmt` files. No migration needed. Prompts are compiled into
the binary via `include_str!` so a rebuild deploys the change.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Longer decomposer prompt causes LLM to over-generate (too many modules, too many tests) | Medium | Low | Spec count is constrained by rule 5 ("Produce 1-3 Specs") and the count_guidance parameter injected by Rust. Template-following behavior is strong. |
| Stricter validator rejects previously-passing Specs, causing retry loops | Medium | Medium | The validator retries are already bounded. If rejection rate spikes, loosen the most aggressive criterion (Failure Modes coverage). |
| Coverage evaluator's contract-coverage check hallucinates missing contracts | Low | Low | The check is additive to existing gap analysis. False positives surface as "gaps" which the decomposer retry addresses. |
| Tier gate misclassifies a contract-neutral Plan as Full, decomposer tries to produce Specs for a Plan with no contracts | Low | Low | The decomposer guidance includes a "minimal contracts" fallback clause. Existing tier-gate prompt already defaults to Full when uncertain, so this path is expected. |

## Scope Boundary: decompose/validate.pmt and decompose/ratify.pmt

These two prompts run after decomposition to check template compliance and parent
coverage respectively. They are intentionally generic - they apply the same checks
to Specs, Phases, and Works. The Spec-specific quality enforcement belongs in
`validator-spec.pmt` (which runs during the Draft->Active validation gate), not in
the decompose-time validate/ratify pass. No changes to these files.

## Open Questions

- [ ] Should the validator criteria be weighted (some blocking, some advisory) or all-or-nothing like today?
- [ ] Should the decomposer prompt include a concrete example of a well-formed Spec section, or is guidance-only sufficient?

## References

- `docs/templates/plan.md` - Plan template (the PRD)
- `docs/templates/spec.md` - Spec template (the Tech Spec)
- `prompts/decompose/spec.pmt` - current decomposer prompt
- `prompts/validator-spec.pmt` - current validator prompt
- `prompts/coverage-plan-specs.pmt` - current coverage evaluator prompt
- `prompts/decompose/work.pmt` - example of a prompt with good interpretive guidance
