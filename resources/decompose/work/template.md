# Work Template

The implementer's complete brief. Everything needed to write code without
asking a question. Every decision has been made upstream. The implementer
executes, not designs. Scale depth to the task.

On retry: copy the description verbatim, append what went wrong. Inputs,
outputs, and acceptance criteria NEVER change.

**Brief mode:** All required sections still apply, but scale down. The
Description is 1-2 sentences. Inputs reference existing code by path
rather than copying full signatures. Implementation Notes are 3-5 steps.
The Work references the Plan directly instead of a Phase.

---

## Parent

Which Phase produced this work item (by ID or name). In Brief mode,
references the Plan directly.

## Description

What this work delivers and what capability it adds. The title and
objective in one paragraph.

Brief mode: Title and objective in 1-2 sentences.

## Inputs

Interface contracts, data models, existing code the implementer receives.
IMMUTABLE on retry - if these need to change, that's a Plan-level revision.

Brief mode: File paths and function names. Link to existing code rather
than copying full signatures.

### Interface Contract
Exact function signatures, copied from the Phase.

### Data Model
Exact schema, copied from the Plan's contracts.

## Outputs

Files created or modified, functions implemented, artifacts generated.
What the implementer produces when the work is complete.

## Constraints

Non-negotiable rules the implementer must follow.

## Implementation Notes

Ordered steps, technical guidance, gotchas. Specific enough to follow
mechanically. Scale with complexity - 5 steps for a simple module, 15
for a complex one.

Brief mode: 3-5 steps.

## Acceptance Criteria

Assert statements. Concrete, testable, executable. The definition of
done. When every assertion passes, the work is complete. Nothing more,
nothing less.

```
assert create_bookmark(db, "Test", "http://x.com") is not None
assert get_bookmark(db, 9999) is None
assert delete_bookmark(db, 1) is False  # already deleted
```

Always as assert statements, never as prose. Scale with complexity.

### Phase Validation Scope
What command to run for THIS phase. What command must NOT be run and why.

Brief mode: Final validation command only.

## Dependencies

Other work items, phases, or external systems that must complete first.

Full mode only. Omit in Brief mode.

---

## Conditional Sections

Include when relevant. Omit when not applicable.

### Design Decisions

Settled decisions from the Spec that affect this work. Listed so the
implementer understands WHY, not so they can revisit. DO NOT RE-LITIGATE.

### Open Questions

Clarifications the upstream documents do not resolve.

---

## Retry Rules

1. COPY the original description VERBATIM
2. APPEND what went wrong and what to do differently
3. NEVER change Inputs or Acceptance Criteria
4. You MAY add to Implementation Notes or Constraints based on learnings
5. Same phase and resource tags as the original
