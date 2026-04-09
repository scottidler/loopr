# Design Document: Dependency Hardening

**Author:** Scott A. Idler
**Date:** 2026-04-09
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Harden dependency resolution in the decomposer and reconciliation system. Fix three classes of bugs: (1) silent drop of unresolved dep titles, (2) cross-type dep contamination (Phase referencing a Spec ID), (3) missing topo_sort_by_deps for display ordering. Also clean up implementation debt from the reactive execution model rollout.

## Problem Statement

### Background

The reactive execution model (v0.1.109) replaced cursor-based phase gating with dependency-driven reconciliation. Records are born Pending and promoted when their deps are satisfied. The decomposer produces deps as title strings, resolves them to IDs, and wires them onto domain records. The reconciler checks those IDs against the appropriate store to determine promotion eligibility.

### Problem

The dep pipeline has three silent failure modes that can cause ordering violations or deadlocks:

**1. Silent drop of unresolved dep titles.**
In `decompose_into()` (decomposer.rs:572-584), if a dep title doesn't match any sibling title, it goes into `unresolved_dep_titles` with a `warn!` log - but nothing ever resolves it. The record gets `dependencies: []` and promotes immediately, defeating ordering. This can happen from LLM typos, case mismatches ("Database Layer" vs "Database layer"), or extra whitespace.

**2. Cross-type dep contamination via `global_title_to_id`.**
The `global_title_to_id` map (decomposer.rs:566) accumulates titles from ALL hierarchy levels. When resolving Phase deps, the lookup falls back from `local_title_to_id` (siblings only) to `global_title_to_id` (everything). If a Phase title collides with a Spec title, the Phase gets a Spec ID in its deps. The reconciler then looks up that Spec ID in the phases store, gets `None`, returns `unwrap_or(false)` - the dep is never satisfied and the Phase is stuck Pending forever. Silent deadlock.

**3. No enforcement that deps are same-type.**
Nothing validates that Spec deps point to Spec IDs (`sp-*`), Phase deps to Phase IDs (`ph-*`), Work deps to Work IDs (`wk-*`). The ID prefix scheme (`sp-`, `ph-`, `wk-`) makes this trivially checkable, but no code does it.

Additionally, the Gemini Architect review of the reactive execution model flagged three implementation debts:

**4. Missing `topo_sort_by_deps` implementation.**
The design doc mandated topological sort for display ordering after removing the `order` field. It was never implemented. Specs and Phases are currently sorted by `created_at`, which can produce incorrect display order when timestamps collide (the decomposer creates records in a tight loop where `now_millis()` returns identical values).

**5. Zombie test `test_records_to_hierarchy_phase_order`.**
After removing the `order` field, the ordering assertions were stripped but the test was kept. It now asserts `phases.len() == 2` twice - proving nothing about ordering.

**6. Gravestone comments in coordinator tests.**
Ten "tests removed" comments left behind from the FSM simplification provide no value and violate the dead-code eradication mandate.

### Goals

- Dep titles that fail to resolve cause decomposition failure (not silent promotion)
- Deps can only reference same-type siblings (Spec->Spec, Phase->Phase, Work->Work)
- Cross-type dep IDs are rejected at every layer: decomposer, IPC handlers, reconciler
- Display ordering uses topological sort by deps, not timestamp
- Zero zombie tests, zero gravestone comments

### Non-Goals

- DAG-shaped dependencies (still linked-list for now)
- Fuzzy title matching with Levenshtein distance or embedding similarity
- Changes to the LLM prompts (they already specify same-level deps correctly)
- Changes to the reconciliation algorithm itself (it's correct given valid deps)

## Proposed Solution

### Overview

Four focused fixes, each independently testable:

1. **Local-only resolution with strict failure** - deps resolve from sibling batch only, case-insensitive fallback, fail on unresolved
2. **Prefix validation** - every dep ID checked against expected type prefix at decomposer, handlers, and reconciler
3. **`topo_sort_by_deps` utility** - topological sort for display ordering, replacing `created_at` sort
4. **Dead code cleanup** - zombie test, gravestone comments, dead `global_title_to_id` wiring

### Fix 1: Local-only resolution with strict failure

Two changes to `decompose_into()`, applied together since they modify the same resolution loop:

**Local-only resolution.** Remove the `global_title_to_id` fallback from dep resolution. Deps resolve from `local_title_to_id` (sibling batch) only. This enforces same-parent deps structurally: the local map only contains siblings from the same decomposition call. A Phase dep title can only resolve to another Phase from the same Spec. A Spec dep title can only resolve to another Spec from the same Plan.

Also remove the `global_title_to_id` parameter from `decompose_into()` entirely. The caller's merge logic in `decompose_hierarchy()` (lines 684-686) becomes dead code since cross-branch resolution was already removed.

**Case-insensitive + whitespace-trimmed fallback.** Build a normalized map (`local_title_to_id_lower`) from the sibling batch: keys are `title.trim().to_lowercase()`. When exact match fails, try the normalized lookup. This catches the most common LLM errors (case mismatch, trailing whitespace).

**Fail on unresolved.** After the resolution loop, if any records have non-empty `unresolved_dep_titles`, bail with an error listing the unresolved titles. The decomposer's existing retry logic (line 502-509) will re-prompt the LLM with the error.

```rust
// Resolution: local-only with case-insensitive fallback
let local_lower: HashMap<String, String> = local_title_to_id
    .iter()
    .map(|(k, v)| (k.trim().to_lowercase(), v.clone()))
    .collect();

for title in &dep_titles {
    if let Some(id) = local_title_to_id.get(title)
        .or_else(|| local_lower.get(&title.trim().to_lowercase()))
    {
        resolved.push(id.clone());
    } else {
        unresolved.push(title.clone());
    }
}

// Strict failure after loop
let unresolved: Vec<_> = final_records
    .iter()
    .filter(|r| !r.unresolved_dep_titles.is_empty())
    .map(|r| format!("'{}' has unresolved deps: {:?}", r.title, r.unresolved_dep_titles))
    .collect();
if !unresolved.is_empty() {
    bail!("Dependency resolution failed:\n{}", unresolved.join("\n"));
}
```

### Fix 2: Prefix validation

Add a `validate_dep_prefix` function that checks each resolved dep ID has the expected prefix for its record type:

```rust
fn expected_dep_prefix(kind: DocKind) -> &'static str {
    match kind {
        DocKind::Spec => "sp-",
        DocKind::Phase => "ph-",
        DocKind::Work => "wk-",
        DocKind::Plan => "pl-",
    }
}
```

Apply at three layers:

**Layer 1: Decomposer** (`records_to_hierarchy`). After copying deps from ChildRecord to domain record, filter out any dep IDs that don't match the expected prefix. Log an error for each rejected dep.

**Layer 2: IPC handlers** (`handle_spec_create`, `handle_phase_create`). Add dep validation matching the existing pattern in `handle_work_create`: existence check in the same-type store, cycle detection, prefix validation. Currently spec and phase create handlers have zero dep validation.

**Layer 3: Reconciler** (defense-in-depth). In `all_hierarchy_deps_terminal`, before looking up the dep in the store, assert the prefix matches. If a cross-type dep is found, log at `error!` level (not `warn!` - a cross-type dep is a data integrity violation that must be visible) and return false (blocking promotion). This catches any dep that leaked through layers 1-2. Note: returning false creates a permanent pending state unless the dep is corrected - the `error!` log ensures this is never silent.

### Fix 3: `topo_sort_by_deps`

Implement a utility function that takes a slice of records with `id` and `dependencies` fields and returns them in dependency order (records with no deps first, then records whose deps have already been placed).

Implement using Kahn's algorithm (BFS topological sort). Kahn's algorithm natively handles disconnected DAGs - it initializes with all nodes of in-degree 0 across all disconnected components and processes them in order. No special handling for disconnected graphs is needed.

When multiple nodes have in-degree 0 simultaneously (tie), break ties by `get_created_at` ascending (earliest created first). Use a min-heap or sort the zero-in-degree queue before processing.

Fall back to `created_at` ordering ONLY if the graph has a cycle, detected as `visited.len() != items.len()` after the algorithm terminates. Log a `warn!` when fallback triggers.

Deps pointing to IDs not in the input set are ignored (already-completed siblings not in the active slice) - these are external edges that don't affect the topological order of the active set.

```rust
pub fn topo_sort_by_deps<T, F, G, C>(items: &[T], get_id: F, get_deps: G, get_created_at: C) -> Vec<&T>
where
    F: Fn(&T) -> &str,
    G: Fn(&T) -> &[String],
    C: Fn(&T) -> i64,
```

The `get_created_at` accessor is used for fallback ordering when topo sort fails (cycle) and for tie-breaking when multiple nodes have zero in-degree simultaneously. Deps pointing to IDs not in the input set are ignored (already-completed siblings not in the active slice).

Apply in:
- `build_state_summary` (coordinator.rs:264, 289) - Phases and Specs display
- `build_execution_status` (coordinator.rs:898, 905) - Active Specs and Phases display
- `find_active_specs_for_plan` and `find_active_phases_for_spec` (generation.rs:156, 170) - query helpers used throughout

Location: `src/domain/sort.rs` (new module under domain, since it operates on domain record traits).

### Fix 4: Dead code cleanup

- Delete `test_records_to_hierarchy_phase_order` in decomposer.rs (zombie test - asserts `phases.len() == 2` twice)
- Delete 10 gravestone comments in `src/agents/coordinator/tests.rs` (lines 690, 709, 775, 907, 936, 939, 1186, 1214, 1500 and any others found by grep)
- Remove dead `global_title_to_id` parameter from `decompose_into()` and caller merge logic in `decompose_hierarchy()` (lines 684-686). The `unresolved_dep_titles` field on `ChildRecord` is kept - it's used transiently during resolution before the strict failure check.

### Implementation Plan

#### Step 1: Dead code cleanup - DONE (commit 1292525)

Deleted zombie test and gravestone comments.

**Files:** `src/decomposer.rs`, `src/agents/coordinator/tests.rs`

#### Step 2: Local-only resolution with strict failure - DONE (commit b273230)

Removed `global_title_to_id` parameter from `decompose_into()`. Dep resolution from local sibling map only with case-insensitive + trimmed fallback. Strict failure on unresolved deps. Dead caller merge logic removed.

**Files:** `src/decomposer.rs`

#### Step 3: Prefix validation

Add `expected_dep_prefix()`. Wire into `records_to_hierarchy` (filter + log error), `handle_spec_create` and `handle_phase_create` (existence check + cycle detection + prefix check, matching `handle_work_create` pattern), and reconciler (defense-in-depth: `error!` log + return false - never silent).

Note: Layer 1 (decomposer) prefix validation is defense-in-depth only - after Fix 2, all dep IDs in `local_title_to_id` are generated by `crate::id::generate_id(target_kind.id_prefix())`, making cross-type deps structurally unreachable via the normal decompose path.

**Files:** `src/decomposer.rs`, `src/daemon/handlers/spec.rs`, `src/daemon/handlers/phase.rs`, `src/agents/coordinator/reconcile.rs`

**Gate:** `otto ci` passes. Unit test: cross-type dep ID is rejected at each layer. Unit test: same-type dep ID passes.

#### Step 4: `topo_sort_by_deps`

Implement the utility using Kahn's algorithm. Kahn's handles disconnected DAGs natively - no special case needed. Use `get_created_at` as tie-breaker (min-heap or pre-sort) when multiple nodes have in-degree 0. Fall back to `created_at` ONLY on cycle (when `visited.len() != items.len()`). Log `warn!` on fallback.

**Files:** `src/domain/sort.rs` (new), `src/domain.rs` (mod declaration), `src/agents/coordinator.rs`, `src/agents/generation.rs`

**Gate:** `otto ci` passes. Unit tests: linked-list, empty, DAG, disconnected components (native - no fallback), cycle falls back to `created_at`, tie-breaking by `created_at`.

## Alternatives Considered

### Alternative 1: Fuzzy title matching (Levenshtein distance)

- **Description:** Use edit distance to find the closest sibling title when exact match fails.
- **Pros:** Catches more LLM errors (typos, pluralization, articles).
- **Cons:** Risk of false matches ("API Tests" matching "API Routes"). Adds a dependency or non-trivial algorithm. Hard to set a threshold that's right for all title lengths.
- **Why not chosen:** Case-insensitive fallback catches the most common error class. If we see other patterns in E2E runs, we can add Levenshtein later with data to set the threshold.

### Alternative 2: Keep `global_title_to_id` fallback with type checking

- **Description:** Allow global resolution but validate the resolved ID's prefix matches the expected type.
- **Pros:** Handles edge cases where a dep title might not be in the local batch.
- **Cons:** There is no valid case for cross-batch resolution. Same-level, same-parent is the invariant. Global fallback is a latent bug source even with prefix checking.
- **Why not chosen:** Local-only resolution is structurally correct. If a dep title isn't in the sibling batch, it's an LLM error that should fail fast.

### Alternative 3: Skip topo_sort, use created_at everywhere

- **Description:** Accept that `created_at` ordering is "good enough" since the decomposer creates records sequentially.
- **Pros:** Zero new code.
- **Cons:** `now_millis()` can return identical timestamps for adjacent records in a tight loop, making sort order non-deterministic. The design doc mandated topo_sort and it was skipped without acknowledgment.
- **Why not chosen:** The timestamp collision is real (observed in test flakiness). Topo sort is the correct solution and is simple for the linked-list case.

## Technical Considerations

### Dependencies

No new external dependencies. `topo_sort_by_deps` uses Kahn's algorithm from standard BFS.

### Performance

All fixes operate on small collections (2-5 Specs, 2-5 Phases per Spec). Topo sort is O(V+E) where V is typically < 10. Negligible.

### Testing Strategy

- **Fix 1 (local-only + strict failure):** Unresolved dep title causes decomposition failure. Case-insensitive match resolves. Whitespace-trimmed match resolves. Non-sibling title fails (not silently dropped).
- **Fix 2 (prefix validation):** Per-layer tests: decomposer rejects cross-type dep ID in `records_to_hierarchy`. Spec/Phase handlers reject cross-type deps. Reconciler blocks promotion on cross-type dep and emits `error!` (not silent).
- **Fix 3 (topo_sort):** Empty list, single item, linked-list (A->B->C), general DAG, disconnected components (natively handled - no fallback), cycle falls back to `created_at` (only cycle triggers fallback, verified by `visited.len() != items.len()`), tie-breaking by `created_at`.
- **Fix 4 (dead code):** Compile check only (deletions).

### Rollout Plan

Single version bump after all steps pass `otto ci`. Internal architecture change, no user-facing impact.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Failing on unresolved deps causes excessive decomposition retries | Medium | Medium | The retry prompt includes the error message, so the LLM can fix the exact titles. Max 1 retry (existing limit). |
| Case-insensitive matching creates false positives | Low | Low | Only matches within the same sibling batch (same type, same parent). Title collisions within a batch are already an error. |
| Topo sort cycle detection triggers on valid data | Low | Medium | Fall back to `created_at` ordering and log a warning. Cycles in deps are already caught by `detect_cycles()` in the decomposer. |
| Spec/Phase handler dep validation breaks existing LLM coordinator actions | Low | Medium | The coordinator creates Works (not Specs/Phases) via actions. Specs/Phases are created by the decomposer which goes through `records_to_hierarchy`, not handlers. Handler validation is defense-in-depth. |

## Open Questions

None. All questions resolved during review passes.

## References

- `docs/design/2026-04-09-reactive-execution-model.md` - parent design doc (Implemented)
- Gemini Architect review (2026-04-09) - flagged missing topo_sort, zombie test, gravestone comments
- `src/decomposer.rs` lines 490-586 - current dep resolution code
- `src/agents/coordinator/reconcile.rs` - reconciliation loop that consumes deps
