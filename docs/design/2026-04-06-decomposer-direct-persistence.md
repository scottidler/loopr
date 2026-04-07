# Design Document: Decomposer Direct Persistence

**Author:** Scott A. Idler
**Date:** 2026-04-06
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Replace the two-pass `Doc -> double_write_old_records -> Plan/Spec/Phase/Work` pipeline with a single pass where the decomposer produces typed domain records directly. This eliminates `double_write_old_records`, `build_all_old_records`, `OldRecord`, and the class of bugs where logic added to IPC handlers is silently skipped during decomposition.

## Problem Statement

### Background

The decomposer pipeline has two layers:

1. **Doc layer** (`decomposer.rs`): the decomposer calls the LLM, validates children, writes `.md` files to a temp staging dir, and returns `Vec<Doc>`. The `Doc` type carries a `markdown` filename field that points to the written file.

2. **Domain layer** (`handlers/doc.rs`): `double_write_old_records` reads back those `.md` files, converts each `Doc` into the appropriate `Plan/Spec/Phase/Work` struct, and writes them directly into the TaskStore and in-memory maps — bypassing every IPC handler.

The IPC handlers (`handle_plan_create`, `handle_spec_create`, etc.) are the "right" path: they contain TaskStore write ordering, in-memory insert, `write_doc_markdown` emission, and event broadcast. `double_write_old_records` reimplements the persistence logic outside this path, diverging every time a handler is touched.

### Problem

**Any logic added to an IPC handler is silently skipped for records created during decomposition.**

Demonstrated failure: `write_doc_markdown` was added to all 12 handler sites in the `docs/loopr/` emission feature, but Plan/Spec/Phase docs were never written because `double_write_old_records` bypasses those handlers. The bug required a runtime observation (only `wk-emu2u.md` appeared in `docs/loopr/`) to detect. The fix was a patch to `double_write_old_records` — adding a 13th call site that will also diverge next time.

This is a structural problem, not a patching problem.

### Goals

- The decomposer produces typed `Plan/Spec/Phase/Work` records and persists them through a single shared function.
- That function is the same one (or identical logic to) the IPC handlers use — so adding logic in one place covers all paths.
- `docs/loopr/` emission, event broadcast, and TaskStore write ordering all happen automatically for decomposition output.
- The `double_write_old_records` / `build_all_old_records` / `OldRecord` code is deleted.

### Non-Goals

- Rewriting the decomposer's LLM calling, validation, or ratification logic.
- Changing the decomposer's internal use of `.md` staging files (those are needed for reading parent content during nested LLM calls).
- Removing the `Doc` domain type (it may still be useful for tracking plan ingestion history).
- Changing the IPC protocol surface (`plan.create`, `spec.create`, etc.) exposed to external callers.
- Changing the `Doc` persistence path (child `Doc` records are still persisted after hierarchy creation).

## Proposed Solution

### Overview

Change `decompose_hierarchy` to return a typed `DecomposedHierarchy` struct instead of `Vec<Doc>`. Add a `persist_hierarchy` function in `handlers/doc.rs` that persists the hierarchy records in dependency order using the same pattern as the IPC handlers (TaskStore write under lock, in-memory insert, `write_doc_markdown`, event broadcast). `double_write_old_records` and its helpers are deleted.

### Architecture

**Before:**
```
accept_plan_markdown
  └── decompose_hierarchy()  -> Vec<Doc>
        └── (staging files in run_dir)
  └── double_write_old_records(stores, plan_doc, markdown, child_docs, run_dir)
        └── build_all_old_records()   <- reads files from run_dir
        └── for each record: store.create() + stores.write_X().insert()
                                        <- no write_doc_markdown, no events
  └── persist_doc() for each child Doc   <- Doc layer, separate from domain layer
```

**After:**
```
accept_plan_markdown
  └── decompose_hierarchy()  -> DecomposedHierarchy { plan, specs, phases, works }
        └── (staging files still used internally, cleaned up inside decomposer)
  └── persist_hierarchy(stores, event_tx, hierarchy)
        └── for each record in order (plan -> specs -> phases -> works):
              store.create() under lock
              stores.write_X().insert()
              write_doc_markdown()    <- same as handlers, always fires
              event_tx.send()         <- same as handlers, always fires
  └── persist_doc() for each child Doc (unchanged)
```

### DecomposedHierarchy type

`DecomposedHierarchy` lives in `src/decomposer.rs` since the decomposer produces and owns it.

```rust
pub struct DecomposedHierarchy {
    pub plan: Plan,
    pub specs: Vec<Spec>,
    pub phases: Vec<Phase>,
    pub works: Vec<Work>,
}
```

### docs_to_hierarchy (private conversion function)

Lives in `src/decomposer.rs` as a private function called at the end of `decompose_hierarchy`. It is a direct port of `build_all_old_records` with typed output instead of `OldRecord`.

Signature:
```rust
fn docs_to_hierarchy(
    plan_doc: &Doc,
    plan_markdown: &str,   // used as Plan.description (the full markdown text)
    child_docs: &[Doc],
) -> eyre::Result<DecomposedHierarchy>
```

Key behaviors (unchanged from `build_all_old_records`):
- `Plan.description = plan_markdown.to_string()` (the full plan markdown, not a summary)
- `Plan.acceptance_criteria` = `AcceptanceCriteria(plan_doc.acceptance_criteria.clone())`
- `Plan` starts with `force_status(PlanStatus::Active)`
- Specs, Phases, Works created in dependency order; parent IDs resolved via a `doc_id -> domain_id` map
- `Work.acceptance_criteria` = `AcceptanceCriteria(child.acceptance_criteria.clone())`
- `Work` starts with `force_status(WorkStatus::Ready)`
- `Work.resource_tags` = empty (the decomposer tool schema has no `resource_tags` field; this is pre-existing behavior, not a regression)
- `Work.dependencies` resolved via the same `doc_to_old` mapping as today

**Note on `plan_markdown` access:** `decompose_hierarchy` is called with `plan: &Doc` which has `plan.markdown` (a filename in the run_dir). The function already reads this file internally for the LLM prompt. The `plan_markdown` content must be passed through (or re-read) to `docs_to_hierarchy`.

### persist_hierarchy

Lives in `src/daemon/handlers/doc.rs`. Called by `accept_plan_markdown` in place of `double_write_old_records`.

```rust
fn persist_hierarchy(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    hierarchy: DecomposedHierarchy,
) -> eyre::Result<()> {
    let repo_path = stores.config.project.repo_path.clone();

    // Persist in dependency order: Plan -> Specs -> Phases -> Works
    macro_rules! persist_record {
        ($store_writer:expr, $coll:expr, $record:expr) => {{
            let r = $record;
            if let Some(store) = &stores.store {
                store.lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create(r.clone())?;
            }
            $store_writer?.insert(r.id.clone(), r.clone());
            if let Err(e) = write_doc_markdown(&repo_path, &r) {
                tracing::warn!("docs/loopr write failed for {}: {}", r.id, e);
            }
            let _ = event_tx.send(DaemonEvent::record_created($coll, &r.id));
        }};
    }

    persist_record!(stores.write_plans(), "plan", hierarchy.plan);
    for r in hierarchy.specs  { persist_record!(stores.write_specs(),  "spec",  r); }
    for r in hierarchy.phases { persist_record!(stores.write_phases(), "phase", r); }
    for r in hierarchy.works  { persist_record!(stores.write_works(),  "work",  r); }

    Ok(())
}
```

This mirrors `handle_plan_create`/`handle_spec_create`/etc. exactly. The `resource_tags.is_empty()` guard in `handle_work_create` is NOT applied here - Works from decomposition always have empty resource_tags, which is the existing behavior.

### Partial failure

`decompose_hierarchy` returns `Result<(DecomposedHierarchy, Option<String>)>`. When a spec branch fails:
- `hierarchy` contains the records from the successful branches only
- `partial_err` is `Some(message)`
- `persist_hierarchy` is called with the partial hierarchy (same as today - only successful docs are persisted)
- The coordinator state is updated with the error and a `decomposition.failed` event is sent

### What is deleted

- `double_write_old_records` (~50 lines)
- `build_all_old_records` (~70 lines)
- `OldRecord` enum + `impl OldRecord` (~15 lines)
- `run_dir` argument to the `double_write_old_records` call site in the background task
- The `run_dir.join(&child.markdown)` file-read in `build_all_old_records` (content comes directly from `ChildEntry.content` or the `Doc` in memory)

Total: ~-135 lines in `handlers/doc.rs`

### What stays

- `decompose_hierarchy` internals: LLM calls, validation, staging files, ratification - unchanged
- Internal `.md` staging files: still needed for reading parent content during nested LLM calls
- The `Doc` type: still persisted via `persist_doc` for ingestion tracking (unchanged)
- All 12 IPC handler sites (plan/spec/phase/work create/update/transition) with their `write_doc_markdown` calls - unchanged
- The `run_dir` temp directory creation in `accept_plan_markdown` - still needed for the decomposer's staging files

## Alternatives Considered

### Alternative 1: Keep double_write_old_records, always patch it

- **Description:** Continue patching `double_write_old_records` whenever a handler gains new logic.
- **Pros:** Minimal code change. No refactor risk.
- **Cons:** Structural divergence is permanent. Every future feature hitting handlers must also hit `double_write_old_records`. The bug will recur.
- **Why not chosen:** Proven to cause bugs. The e2e run on 2026-04-06 demonstrated the failure mode in production.

### Alternative 2: Route through IPC bridge instead of direct store writes

- **Description:** Have `double_write_old_records` call `plan.create`, `spec.create`, etc. via the IPC bridge rather than writing to the store directly.
- **Pros:** Reuses exact handler code. No type conversion needed.
- **Cons:** IPC round-trips for O(N) records during decomposition. Each call acquires locks, broadcasts events, and serializes/deserializes JSON. Adds latency proportional to hierarchy size. The IPC bridge adds concurrency complexity inside an already-concurrent background task.
- **Why not chosen:** Performance cost is unnecessary. `persist_hierarchy` achieves the same correctness guarantee with a direct call pattern that mirrors the handler logic without the IPC overhead.

### Alternative 3: Add `write_doc_markdown` to the `Record` trait

- **Description:** Make `write_doc_markdown` a required method on `Record`, called automatically by the TaskStore on every `create`.
- **Pros:** Truly impossible to miss - every create triggers it.
- **Cons:** Requires `write_doc_markdown` to be in the TaskStore layer (an external crate), or requires passing `repo_path` into every `store.create()` call. `repo_path` is application-level config, not store-level config. Couples persistence to filesystem layout in the wrong layer.
- **Why not chosen:** Architectural boundary violation. The TaskStore crate should not know about `docs/loopr/` file layout.

## Technical Considerations

### Dependencies

- `decomposer.rs` gains a dependency on `domain::{plan, spec, phase, work}` to construct typed records. It already transitively knows the domain shapes; this makes it explicit.
- `handlers/doc.rs` loses the `double_write_old_records`/`build_all_old_records`/`OldRecord` code. Roughly -150 lines.
- `decomposer.rs` gains a `docs_to_hierarchy` conversion function. Roughly +80 lines.
- Net: ~70 lines deleted.

### Ordering Constraint

`persist_hierarchy` must insert records in dependency order: Plan first, then Specs (parent_id = plan), then Phases (parent_id = spec), then Works (parent_id = phase). This is the same ordering that `build_all_old_records` already enforces via the `specs.iter().chain(phases.iter()).chain(works.iter())` pattern.

### Partial Failure

`decompose_hierarchy` can return a partial result when one or more spec branches fail. `persist_hierarchy` must handle this correctly - persisting the successfully-produced records while propagating the partial error. This mirrors what `double_write_old_records` does today (it receives only the successful child docs).

### Testing Strategy

- Existing tests for `decompose_hierarchy` that use `skip_decompose=true` continue to work (they bypass decomposition entirely).
- The `double_write_old_records` tests in `handlers/doc.rs` are deleted (the function no longer exists).
- New tests for `docs_to_hierarchy` conversion in `decomposer.rs`: given a Vec<Doc> with known structure, assert the Plan/Spec/Phase/Work records are correctly constructed.
- Integration: the e2e runs will confirm `docs/loopr/` is populated for all record types after decomposition.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Conversion logic bugs in `docs_to_hierarchy` | Medium | Medium | Port `build_all_old_records` logic directly; unit tests cover all four types and the doc_id->domain_id mapping |
| `Plan.description` content change | Low | Low | `plan_markdown` passed explicitly; same as today |
| Doc ID vs domain ID mismatch in dependencies | Medium | High | The `doc_to_old` mapping pattern is preserved exactly in `docs_to_hierarchy`; test with multi-work phases that have dependencies |
| Partial failure handling regression | Low | Medium | Existing partial-failure test coverage; `persist_hierarchy` receives only the successfully-produced records, same as today |
| Tests that call `double_write_old_records` directly break | Low | Low | Any such tests are deleted with the function; replace with `docs_to_hierarchy` + `persist_hierarchy` tests |
| `handle_work_create` resource_tag guard bypassed | N/A | N/A | Pre-existing behavior; `persist_hierarchy` intentionally skips this guard (decomposer works have empty resource_tags by design) |
| `plan_markdown` content not accessible in `docs_to_hierarchy` | Medium | Medium | `decompose_hierarchy` already reads the plan file for the LLM prompt; pass the content through or re-read at call site |

## Implementation Plan

### Phase 1: Add DecomposedHierarchy and docs_to_hierarchy

- Define `DecomposedHierarchy` struct in `src/decomposer.rs`
- Add private `docs_to_hierarchy(plan_doc, plan_markdown, child_docs) -> Result<DecomposedHierarchy>` in `src/decomposer.rs`
- This is a direct port of `build_all_old_records` with typed output
- Tests for the conversion function

### Phase 2: Change decompose_hierarchy return type

- Change `decompose_hierarchy` to return `Result<(DecomposedHierarchy, Option<String>)>` instead of `Result<(Vec<Doc>, Option<String>)>`
- Call `docs_to_hierarchy` inside `decompose_hierarchy` after all branches complete
- Keep the internal `Vec<Doc>` workflow unchanged (staging files, Doc construction, etc.)

### Phase 3: Add persist_hierarchy, delete double_write_old_records

- Add `persist_hierarchy(stores, event_tx, hierarchy)` to `handlers/doc.rs`
- Update `accept_plan_markdown` to call `persist_hierarchy` instead of `double_write_old_records`
- Delete `double_write_old_records`, `build_all_old_records`, `OldRecord`
- Run `otto ci`

## Binding Decisions

These are resolved - no open questions remain.

1. **`docs_to_hierarchy` lives in `decomposer.rs`**, not `handlers/doc.rs`. The decomposer owns the conversion from its internal `Vec<Doc>` representation to typed output.

2. **`persist_hierarchy` is not a wrapper around IPC handlers.** It mirrors the handler pattern directly (TaskStore write, in-memory insert, `write_doc_markdown`, event broadcast) without IPC round-trips. Avoids O(N) JSON serialize/deserialize and lock contention from N separate requests.

3. **`write_doc_markdown` failure remains advisory** in `persist_hierarchy`. A disk error must not abort decomposition and leave the coordinator stuck in `Decomposing` indefinitely.

4. **The `run_dir` temp directory stays** in `accept_plan_markdown`. The decomposer still needs it for staging files during nested LLM calls. It is not the caller's responsibility to clean it up - use `std::env::temp_dir()` so the OS handles it.

5. **No `resource_tags` in `ChildEntry`.** Works created from decomposition have empty `resource_tags`. This is pre-existing behavior and is not changed by this refactor. `persist_hierarchy` does not enforce the `resource_tags.is_empty()` guard from `handle_work_create`.

## References

- `src/daemon/handlers/doc.rs` - `double_write_old_records`, `build_all_old_records`, `OldRecord`
- `src/decomposer.rs:628` - `decompose_hierarchy` entry point
- `src/decomposer.rs:472` - `decompose_into` internal function
- `src/daemon/handlers/plan.rs` - `handle_plan_create` (the pattern `persist_hierarchy` mirrors)
- `docs/design/2026-04-06-docs-loopr-markdown-emission.md` - previous work, root cause of this doc
