# Design Document: Brief Mode Context Hierarchy Fix

**Author:** Scott A. Idler
**Date:** 2026-04-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The `purge-generation` refactor introduced Brief mode decomposition but left
`ContextBuilder::load_work_hierarchy` unupdated. It still assumes a strict 4-tier
Work->Phase->Spec->Plan chain. In Brief mode, Work items are parented directly to a Plan;
the Phase lookup fails with `phase not found: <plan-id>`, deadlocking all implementers and
reviewers before any LLM call is made. This fix completes the refactor.

## Problem Statement

### Background

Loopr supports two decomposition modes: Full (Plan->Spec->Phase->Work) and Brief
(Plan->Work directly). The Coordinator and Decomposer handle both. `ContextBuilder` does not.

### Problem

`load_work_hierarchy` unconditionally looks up `work.parent_id` in the phases store. In Brief
mode the parent is a Plan ID, the lookup returns `None`, and the agent exits with
`phase not found`. The run deadlocks.

### Goals

- `load_work_hierarchy` resolves hierarchy correctly in both Full and Brief mode.
- `load_bundle_hierarchy` works correctly in both modes (it delegates, so the fix propagates).
- Sibling works are filtered by the correct parent regardless of tier.
- `scope_ids` only includes tiers that were actually resolved.

### Non-Goals

- No changes to Coordinator, Decomposer, or domain types.
- No new decomposition tiers.

## Proposed Solution

### Overview

Branch on the prefix of `work.parent_id`. If `pl-`, take the Brief path (Plan only). If `ph-`,
take the Full path (existing logic). Rename the struct field `phase_id` to `parent_id` since it
now holds whichever parent tier applies.

### Architecture

All changes confined to `src/agents/context.rs`.

### Data Model

No changes to persisted types.

### API Design

- `ContextBuilder` field rename: `phase_id` -> `parent_id`
- `load_work_hierarchy` behavior branches on parent prefix, signature unchanged.

### Implementation Plan

**Step 1 — Brief path in `load_work_hierarchy`**

After reading the Work and extracting `parent_id`, branch on prefix:

```
if parent_id starts with "pl-":
    read Plan from plans store
    self.plan = Some((plan_title, plan_desc))
    self.spec = None  (already None, leave it)
    self.phase = None (already None, leave it)
    self.work = Some((wi_title, wi_desc))
    self.work_id = Some(work_id)
    self.parent_id = Some(plan_id)
    self.scope_ids = [(work_id, Work), (plan_id, Plan)]
else if parent_id starts with "ph-":
    existing Full path (Phase -> Spec -> Plan)
    self.scope_ids = [(work_id, Work), (phase_id, Phase), (spec_id, Spec), (plan_id, Plan)]
else:
    bail!("unexpected parent prefix for work {}: {}", work_id, parent_id)
```

**Step 2 — Rename `phase_id` -> `parent_id`**

Three sites in `src/agents/context.rs`:
- Struct field declaration (~line 251)
- `Self { ... }` initializer in `new()` (~line 294)
- Assignment `self.phase_id = Some(...)` in `load_work_hierarchy` (~line 379, now Step 1 above)
- Use in sibling filter (~line 590)

**Step 3 — Sibling-works filter (no logic change needed)**

The filter `wi.parent_id == *phase_id` becomes `wi.parent_id == *parent_id` after the rename.
This is already semantically correct: in both modes it matches Works that share the same direct
parent as the current Work.

**Step 4 — Tests**

Add to `src/agents/context/tests.rs`:
- `test_load_work_hierarchy_brief`: Work with `pl-` parent, no Phase/Spec in stores, asserts
  `plan` is Some, `spec`/`phase` are None, `scope_ids` has two entries.
- Confirm existing Full mode tests still pass.

## Alternatives Considered

### Alternative 1: Fall back on phase-not-found error

- **Description:** Catch the error and retry with Plan lookup.
- **Pros:** Minimal diff.
- **Cons:** Hides structural difference behind error handling. The Spec lookup would then also
  fail and need the same treatment — doubling the error-handling complexity with no gain.
- **Why not chosen:** Error recovery is the wrong tool for a known structural variant.

## Technical Considerations

### Dependencies

None. All changes in `src/agents/context.rs`.

### Testing Strategy

- Add `test_load_work_hierarchy_brief` in `src/agents/context/tests.rs`: Work with `pl-` parent,
  no Phase/Spec in stores. Assert `plan` is Some, `spec`/`phase` are None, `scope_ids` has
  exactly two entries (Work + Plan).
- Existing Full mode tests must continue to pass unchanged.
- `e2e rust-version` exercises Brief mode end-to-end (primary regression signal).
- `e2e react-todo` exercises Full mode end-to-end (regression guard for the Full path).

### Rollout Plan

Fix on `v3`. Verified with `otto ci` + `e2e rust-version`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Full mode regression | Low | High | Existing tests cover Full path; `e2e react-todo` exercises Full mode |
| Unexpected parent prefix (malformed ID or future tier) | Low | Medium | `else bail!(...)` returns a clear error rather than silently using wrong data |

## Open Questions

None. The `sp-` (Spec as direct Work parent) case is handled by the `else bail!` guard. The
Decomposer never produces it; if it ever does, the error message will be explicit.

## References

- `src/agents/context.rs` - `load_work_hierarchy` (line 311), sibling section (line 590)
- `src/decomposer.rs:480` - `decompose_hierarchy` Brief/Full branch
- `src/domain/plan.rs:41` - `Tier` enum (Full / Brief)
- Architectural audit: e2e run `rust-version` 2026-04-05, session `20260405T215220`
