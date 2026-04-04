# Design Document: Phase 4 Review Fixes

**Author:** Scott Idler + Claude
**Date:** 2026-04-04
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Fix five real bugs identified in the Phase 4 review plus revert broken partial changes to decomposer/mod.rs. The primary fix is making doc.accept double-write to old stores so the Coordinator can actually execute plans entered via the chat funnel.

## Problem Statement

### Background

Phase 4 of the Document Architecture refactor shipped doc.accept, doc.inject, and updated seed_manifest. Two external reviews identified real bugs. An unauthorized Gemini edit then broke decomposer/mod.rs (added `brief: bool` to `decompose()` signature without updating child_kind or call sites, plus left a stray brace - code no longer compiles).

### Problem

1. **doc.accept pipeline is broken.** Writes only to `stores.docs`. Coordinator reads `stores.plans`/`specs`/`phases`/`works`. Plans via chat funnel are invisible to execution.

2. **Brief mode produces wrong DocKind.** `decompose(plan, ...)` calls `child_kind(DocKind::Plan)` which returns `DocKind::Spec`. Brief mode children are Specs, not Works.

3. **Gemini broke decomposer/mod.rs.** Added `brief: bool` to `decompose()` but didn't update `child_kind` signature, didn't update 6 call sites, left extraneous brace. Code doesn't compile.

4. **Missing Doc in test helpers.** All three TaskStore test helpers omit `rebuild_indexes::<Doc>()`.

5. **Double Arc in test.** `test_persist_doc_in_memory` wraps `Arc<Stores>` in another `Arc`.

6. **Duplicate persist helpers.** `persist_doc` (doc.rs) and `store_doc` (coordinator.rs) are identical.

### Goals

- G1: Code compiles again (revert Gemini damage)
- G2: doc.accept produces a working end-to-end pipeline
- G3: Brief mode produces Works, not Specs
- G4: Test helpers include Doc indexes
- G5: Fix double Arc
- G6: Single persist helper in common.rs

### Non-Goals

- Migrating the Coordinator to read from stores.docs
- Removing old Plan/Spec/Phase/Work stores
- Removing generation logic from Coordinator

## Proposed Solution

### Overview

Six fixes in dependency order:

| Phase | Fix | Complexity |
|-------|-----|------------|
| 0 | Revert Gemini damage | Trivial (git checkout) |
| 1 | Brief mode | Medium (new internal function) |
| 2 | Consolidate persist helper | Small (move to common.rs) |
| 3 | doc.accept double-write | Medium (create old records from Docs) |
| 4 | Test helper indexes | Trivial |
| 5 | Double Arc | Trivial |

### Phase 0: Revert Gemini Damage

```bash
git checkout HEAD -- src/decomposer/mod.rs
```

Restores the last committed state where `decompose()` has no `brief` param and the code compiles.

### Phase 1: Brief Mode Fix

**Approach:** Extract the core decomposition logic into an internal `decompose_into()` that takes an explicit target `DocKind`. Keep `decompose()` signature unchanged.

```rust
// New internal function - the actual implementation
fn decompose_into(
    parent: &Doc,
    target_kind: DocKind,
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &dyn HttpClient,
) -> Result<Vec<Doc>> {
    // All current decompose() logic, but uses target_kind instead of child_kind()
    // build_decompose_prompt uses target_kind for template selection
    // Doc::new uses target_kind for the child records
}

// Existing function - unchanged signature, thin wrapper
pub fn decompose(
    parent: &Doc,
    run_dir: &Path,
    config: &DecomposerConfig,
    http_client: &dyn HttpClient,
) -> Result<Vec<Doc>> {
    let ck = child_kind(parent.kind)
        .ok_or_else(|| eyre!("cannot decompose a {} document", parent.kind))?;
    decompose_into(parent, ck, run_dir, config, http_client)
}
```

Update `build_decompose_prompt` to select template by target kind (the output type), not parent kind:

```rust
fn build_decompose_prompt(target_kind: DocKind, parent_content: &str) -> Result<String> {
    let template = match target_kind {
        DocKind::Spec => &prompts.decompose_spec,
        DocKind::Phase => &prompts.decompose_phase,
        DocKind::Work => &prompts.decompose_work,
        DocKind::Plan => bail!("cannot decompose into Plan"),
    };
    // ...
}
```

Update `decompose_hierarchy` brief mode:

```rust
if brief {
    // Brief mode: Plan -> Works directly (skip Spec/Phase)
    let works = decompose_into(plan, DocKind::Work, run_dir, config, http_client)?;
    all_docs.extend(works);
}
```

**Why not change decompose() signature:** `decompose()` is a public API called from 4 sites in decompose_hierarchy plus tests. Threading `brief` through every call is wrong because brief is a hierarchy-level decision, not a per-decomposition decision. The individual `decompose(spec, ...)` and `decompose(phase, ...)` calls in full mode don't care about brief - they always produce the natural child kind. Only the top-level Plan->Work shortcut needs the override.

### Phase 2: Consolidate Persist Helper

Move `persist_doc` from doc.rs to common.rs. Delete `store_doc` from coordinator.rs. Both files import from `super::common::persist_doc`.

```rust
// In common.rs:
pub(super) fn persist_doc(
    stores: &Arc<Stores>,
    doc: Doc,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> eyre::Result<()> {
    let id = doc.id.clone();
    if let Some(store_arc) = &stores.store {
        store_arc
            .lock()
            .map_err(|_| eyre!("taskstore lock poisoned"))?
            .create(doc.clone())
            .map_err(|e| eyre!("Failed to persist Doc {}: {}", id, e))?;
    }
    stores.write_docs()?.insert(id.clone(), doc);
    let _ = event_tx.send(DaemonEvent::record_created("docs", &id));
    Ok(())
}
```

### Phase 3: doc.accept Double-Write

After the Decomposer produces child Docs, `accept_plan_markdown` also creates old-style Plan/Spec/Phase/Work records so the Coordinator can execute.

**Algorithm:**

```
doc_to_old_id: HashMap<String, String>  // Doc ID -> old record ID

1. Create plan Doc (already done)
2. Create old Plan record from plan markdown
   - Plan::new(title, markdown_content, acceptance_criteria_joined)
   - force_status(Active)
   - persist to stores.plans + TaskStore
   - doc_to_old_id[plan_doc.id] = old_plan.id

3. Run Decomposer -> child_docs (already done)

4. For each child Doc, read .md from run_dir, create old record:
   - Look up old parent ID: doc_to_old_id[child.parent_id]
   - Match child.kind:
     - Spec: Spec::new(old_parent_id, title, content), force_status(Active)
     - Phase: Phase::new(old_parent_id, title, content, order), force_status(Active)
     - Work: Work::new(old_parent_id, title, content), force_status(Ready)
   - Persist to old stores + TaskStore
   - doc_to_old_id[child.id] = old_record.id
```

**Phase order computation:** Count Phases per parent Spec, incrementing. Works don't need order.

**Acceptance criteria for Works:** Copy from `child_doc.acceptance_criteria` to `old_work.acceptance_criteria`.

**Dependencies for Works:** Doc dependencies reference Doc IDs. Old Work dependencies reference old Work IDs. Map using `doc_to_old_id`.

**Error handling:** If old record creation fails, log a warning but don't fail the handler. The Doc records are the source of truth; old records are transitional.

### Phase 4: Test Helper Indexes

Add `store.rebuild_indexes::<Doc>().unwrap();` to:
- `test_stores_with_taskstore()`
- `test_stores_with_validator()`
- `test_stores_with_validator_strictness()`

### Phase 5: Double Arc

In `test_persist_doc_in_memory`, change:

```rust
// Before:
let stores = test_stores();
let stores = std::sync::Arc::new(stores);  // Double Arc

// After:
let stores = test_stores();  // Already Arc<Stores>
```

## Alternatives Considered

### Alternative 1: Thread brief through decompose() signature

- **Description:** Add `brief: bool` to `decompose()` and `child_kind()` (what Gemini attempted)
- **Pros:** Single function handles both modes
- **Cons:** Breaks all 6 call sites. brief is a hierarchy-level concern, not per-decomposition. The `decompose(spec, ...)` calls in full mode don't care about brief. Forces every caller to pass a flag they don't use.
- **Why not chosen:** Wrong abstraction level. Brief is a `decompose_hierarchy` concern.

### Alternative 2: Mutate Doc kind after decompose returns

- **Description:** Call `decompose(plan, ...)` in brief mode (produces Specs), then change each Doc's kind to Work
- **Pros:** No new functions needed
- **Cons:** The LLM prompt used `decompose_spec.pmt` template, so the content is spec-shaped not work-shaped. Renaming the kind doesn't fix the content. The .md files would have spec-level content labeled as Works.
- **Why not chosen:** Semantic mismatch between content and kind.

### Alternative 3: Skip double-write, migrate Coordinator to read Docs

- **Description:** Instead of double-writing, make the Coordinator read from stores.docs
- **Pros:** No transitional code. Clean architecture.
- **Cons:** Massive scope change. Coordinator, all handlers, all tests, TUI views all read from old stores. This is the eventual end state but is a separate multi-phase project.
- **Why not chosen:** Too large for a bug fix. Double-write is the correct transitional approach.

## Technical Considerations

### Dependencies

- No new crates
- Uses existing Plan/Spec/Phase/Work constructors and force_status methods

### Performance

- Double-write adds negligible overhead (in-memory HashMap inserts + TaskStore appends)
- Reading .md files back from run_dir for title extraction is trivial I/O

### Testing Strategy

- Existing doc.accept tests (`test_doc_accept_skip_decompose_creates_doc`) should verify old records are also created
- Add assertion: `stores.read_plans()` has 1 entry after doc.accept
- Brief mode: add test in decomposer that verifies `decompose_into(plan, DocKind::Work, ...)` produces Work-kind Docs (requires MockLlm)
- All existing tests must continue to pass after the refactor

### Rollout Plan

Phase 0 is a git revert. Phases 1-5 can land in a single commit since they're all bug fixes.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| decompose_into prompt produces wrong-shaped content for brief Works | Medium | Medium | Use decompose_work.pmt which already asks for work-level output |
| Double-write creates ID mapping bugs (Doc ID vs old ID) | Low | High | HashMap tracks mapping; test assertions verify both stores |
| Reverting Gemini changes loses any valid edits | Low | Low | Only decomposer/mod.rs was touched; review diff before checkout |

## Open Questions

- [x] **Should decompose() signature change?** RESOLVED: No. Extract decompose_into() with explicit target kind.
- [x] **Should old records be created before or after decomposition?** RESOLVED: After. We need the Decomposer's output to know what Specs/Phases/Works to create.

## References

- `docs/design/2026-04-04-document-architecture.md` - parent design doc
- `src/decomposer/mod.rs` - Decomposer implementation
- `src/daemon/handlers/doc.rs` - doc.accept/inject handlers
- `src/daemon/handlers/coordinator.rs` - seed_manifest with Doc creation
