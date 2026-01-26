# Rule of Five: Structured Plan Refinement

**Author:** Scott A. Idler
**Date:** 2026-01-15 (updated 2026-01-25)
**Status:** Active
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

The Rule of Five is a structured review methodology for creating high-quality Plan documents. It mandates 5 sequential review passes, each focusing on a specific quality dimension. This prevents the common failure mode of "looks good to me" reviews that miss critical gaps.

---

## The Problem

LLMs (and humans) tend to produce documents that are:
- **Incomplete** - missing edge cases, error handling, integration points
- **Ambiguous** - multiple valid interpretations
- **Over-confident** - assumes happy path, ignores failure modes
- **Shallow** - describes "what" without "how" or "why not"

A single review pass catches obvious issues but misses subtle problems. Multiple unfocused passes waste effort reviewing the same things.

---

## The Solution: Five Focused Passes

Each pass has a **single focus**. The reviewer (LLM or human) ignores other concerns during that pass.

### Pass 1: Completeness

**Question:** Is anything missing?

**Check for:**
- All required sections present
- All features mentioned in summary have detail sections
- Error handling documented
- Configuration options listed
- Dependencies identified
- Migration path described (if applicable)

**Common gaps:**
- "And other features" hand-waving
- Missing non-functional requirements (performance, security)
- No rollback plan

### Pass 2: Correctness

**Question:** Is anything wrong?

**Check for:**
- Logical errors in proposed approach
- Invalid assumptions
- Technical impossibilities
- Incorrect dependencies
- Wrong data types or formats

**Common errors:**
- Assuming API exists that doesn't
- Race conditions in described flows
- Circular dependencies

### Pass 3: Edge Cases

**Question:** What could go wrong?

**Check for:**
- Error handling for each operation
- Timeout behavior
- Partial failure scenarios
- Resource exhaustion
- Malicious input handling
- Concurrent access issues

**Common misses:**
- Network failures
- Disk full
- Permission denied
- Invalid user input
- Clock skew

### Pass 4: Architecture

**Question:** Does this fit the larger system?

**Check for:**
- Consistency with existing patterns
- Integration points with other components
- Impact on existing functionality
- Scalability implications
- Technical debt being created

**Common issues:**
- Reinventing existing utilities
- Breaking encapsulation
- Creating circular imports
- Inconsistent naming conventions

### Pass 5: Clarity

**Question:** Can someone implement this unambiguously?

**Check for:**
- Precise language (no "should", "might", "could")
- Concrete examples
- Clear acceptance criteria
- No undefined terms
- Measurable outcomes

**Common problems:**
- "Make it fast" (fast is not measurable)
- "Handle errors appropriately" (appropriate is undefined)
- Jargon without definition

---

## Implementation in Loopr

The Plan loop implements Rule of Five via the `review-pass` template variable:

```yaml
plan:
  prompt-template: |
    ## Review Pass
    This is review pass {{review-pass}} of 5.

    Focus areas by pass:
    - Pass 1: Completeness - Are all sections filled? Missing requirements?
    - Pass 2: Correctness - Logical errors? Wrong assumptions?
    - Pass 3: Edge Cases - What could go wrong? Error handling?
    - Pass 4: Architecture - Does this fit the larger system?
    - Pass 5: Clarity - Is it implementable? Ambiguous sections?
```

### Pass Tracking

The Plan loop tracks which pass it's on via the `review-pass` context variable. The loop advances the pass when validation passes for the current focus area.

```rust
// Pseudo-code for pass advancement
if validation_passes && current_pass < 5 {
    context.review_pass += 1;
    // Continue to next pass
} else if validation_passes && current_pass == 5 {
    // All passes complete, Plan is ready
    mark_complete();
}
```

### Validation Per Pass

Each pass has pass-specific validation. The validator script can check:

```bash
# Example: .loopr/validators/plan-pass-1.sh
#!/bin/bash
# Pass 1: Completeness check

PLAN_FILE=$1

# Check required sections exist
for section in "Summary" "Goals" "Non-Goals" "Proposed Solution" "Risks"; do
    if ! grep -q "^## $section" "$PLAN_FILE"; then
        echo "Missing required section: $section"
        exit 1
    fi
done

# Check no placeholder text
if grep -qi "TODO\|TBD\|FIXME" "$PLAN_FILE"; then
    echo "Found placeholder text"
    exit 1
fi

exit 0
```

---

## Prompt Files

For more sophisticated Plan generation, use dedicated prompt files.

### Location

```
prompts/
├── plan-system.pmt           # System prompt for Plan loop
├── plan-pass-1.pmt           # Pass 1 specific instructions
├── plan-pass-2.pmt           # Pass 2 specific instructions
├── plan-pass-3.pmt           # Pass 3 specific instructions
├── plan-pass-4.pmt           # Pass 4 specific instructions
└── plan-pass-5.pmt           # Pass 5 specific instructions
```

### Example: plan-pass-1.pmt

```
You are reviewing a Plan document for COMPLETENESS.

This is Pass 1 of 5. Focus ONLY on completeness. Do not comment on correctness,
edge cases, architecture, or clarity - those are for later passes.

## Completeness Checklist

For this Plan to pass the completeness check, it must have:

1. **Summary** (2-3 sentences)
   - What is being built?
   - Why is it needed?
   - What's the scope boundary?

2. **Goals** (3-7 bullet points)
   - What will be true when this is done?
   - Measurable where possible

3. **Non-Goals** (2-5 bullet points)
   - What is explicitly out of scope?
   - What might readers assume is included but isn't?

4. **Proposed Solution**
   - High-level approach
   - Key components
   - Data flow

5. **Implementation Approach**
   - Phases (if multi-phase)
   - Dependencies between phases
   - Validation criteria per phase

6. **Risks and Mitigations**
   - What could go wrong?
   - How will you prevent/detect/recover?

7. **Open Questions** (if any remain)
   - Questions that need answers before implementation
   - Who can answer them?

## Your Task

Review the Plan below. For each missing or incomplete section:
1. Note what's missing
2. Suggest specific content to add

If the Plan is complete, say "PASS: All required sections present and filled."

---

{{current-plan}}
```

### Example: plan-pass-3.pmt

```
You are reviewing a Plan document for EDGE CASES.

This is Pass 3 of 5. Focus ONLY on edge cases and error handling.
Assume the Plan is complete and correct - those were checked in earlier passes.

## Edge Case Categories

For each component/operation in the Plan, consider:

### Resource Failures
- Network unavailable
- Disk full
- Memory exhausted
- File locked by another process
- Permission denied

### Timing Issues
- Timeout during operation
- Partial completion before failure
- Concurrent access from multiple processes
- Clock skew between systems

### Data Issues
- Empty input
- Malformed input
- Extremely large input
- Unicode/encoding issues
- Null/missing fields

### State Issues
- Operation interrupted mid-way
- Retry after partial failure
- Stale cache/data
- Version mismatch

## Your Task

For each operation described in the Plan:
1. Identify potential edge cases
2. Check if the Plan addresses them
3. If not, suggest specific handling

Format your response as:
- **[Component/Operation]**: [Edge case] - [Current handling or "NOT ADDRESSED"]

If all edge cases are addressed, say "PASS: Edge cases adequately covered."

---

{{current-plan}}
```

---

## Usage in Practice

### Manual Plan Creation

```bash
# Start Plan refinement loop
loopr run plan "Add OAuth authentication"

# Loop runs 5 passes automatically
# Each pass focuses on one quality dimension
# Plan is marked ready when all 5 pass
```

### Viewing Pass Progress

```bash
loopr metrics --loop-type plan
# Shows iteration counts and pass progress
```

---

## Why Five?

**Why not 3?** Three passes miss nuance. Combining "correctness" and "edge cases" leads to shallow edge case review.

**Why not 7?** Diminishing returns. Five covers the critical dimensions. Additional passes would overlap.

**Why sequential?** Each pass builds on previous. Can't check clarity until content is complete. Can't check architecture until logic is correct.

---

## Comparison to Other Methods

| Method | Passes | Focus | Problem |
|--------|--------|-------|---------|
| **Single review** | 1 | Everything | Misses subtle issues |
| **Checklist review** | 1 | Predefined items | Misses context-specific issues |
| **Pair review** | 1 | Two perspectives | Still single-pass thinking |
| **Rule of Five** | 5 | One dimension per pass | Thorough, structured |

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy and Plan loop
- [loop-validation.md](loop-validation.md) - Validation strategies
- [loop-config.md](loop-config.md) - Loop type configuration
- [progress-strategy.md](progress-strategy.md) - Cross-iteration state
