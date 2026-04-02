# Design Document: Implementer Context Enrichment

**Author:** Scott Idler
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The ContextBuilder drops acceptance_criteria, resource_tags, and dependency outputs when assembling Implementer context. This causes Implementers to guess APIs, touch wrong files, and fail acceptance criteria they never saw. Fix: enrich `load_work_hierarchy` and `build` to surface these fields.

## Problem Statement

### Background

The ContextBuilder (`src/agents/context.rs:227-255`) assembles the user message for every agent role. It traverses the Work -> Phase -> Spec -> Plan hierarchy via `load_work_hierarchy` (line 297), but only extracts `(title, description)` tuples at each level.

The Work struct (`src/domain/work.rs:43-60`) has three additional fields critical for implementation:
- `resource_tags: Vec<String>` - file paths the implementer is scoped to
- `acceptance_criteria: Vec<String>` - success criteria to satisfy
- `dependencies: Vec<String>` - IDs of prerequisite Work items

### Problem

None of these reach the LLM. The implementer.pmt (line 58) says "resource_tags in your Work define your allowed files" but the actual tags are never rendered into the context. The implementer operates blind:

1. **No acceptance criteria** - cannot verify its own work against concrete criteria
2. **No resource_tags** - the prompt references them but they're absent from the message
3. **No dependency visibility** - when Work B depends on Work A (completed), the implementer for B has zero knowledge of what API or file structure A produced

This directly caused the lua-todo E2E failure: `test_todo.lua` was written expecting `TodoStore.new()` and `store:mark_done()`, but the dependency `todo.lua` exported a flat `todo.add_task()` / `store:done()` API. The implementer had no way to discover this mismatch.

### Goals

- Surface acceptance_criteria, resource_tags, and dependency metadata in the Implementer's assembled context
- Enable downstream Implementers to discover the API surface of completed dependencies
- Fit within the existing hierarchy token budget (2000 tokens for Implementer)

### Non-Goals

- Changing how dependencies are declared or resolved
- Adding new dependency types (interface contracts, API schemas)
- Modifying the Reviewer or Coordinator context explicitly (Reviewer benefits automatically since `load_bundle_hierarchy` delegates to `load_work_hierarchy`)
- Auto-extracting API signatures from dependency code (future work)

## Proposed Solution

### Overview

Enrich `load_work_hierarchy` to extract Work.acceptance_criteria, Work.resource_tags, and Work.dependencies. For completed dependencies, also extract their title and resource_tags. Render all of this in the hierarchy section of `build()`.

### Changes to ContextBuilder

**New fields on the struct:**

```rust
// In ContextBuilder struct (after existing `work` field):
work_acceptance_criteria: Vec<String>,
work_resource_tags: Vec<String>,
dependency_summaries: Vec<DependencySummary>,
```

**New helper struct** (private to context module, not serialized):

```rust
struct DependencySummary {
    title: String,
    status: String,
    resource_tags: Vec<String>,
}
```

### Changes to load_work_hierarchy

Extract all Work fields and resolve dependencies in a single read_works() guard for a consistent snapshot:

```rust
let (wi_title, wi_desc, phase_id, wi_ac, wi_rt, dep_summaries) = {
    let guard = self.stores.read_works()?;
    let wi = guard.get(work_id).ok_or_else(|| eyre!("work not found: {}", work_id))?;
    let deps: Vec<DependencySummary> = wi.dependencies.iter().filter_map(|dep_id| {
        guard.get(dep_id).map(|dep| DependencySummary {
            title: dep.title.clone(),
            status: dep.status.to_string(),
            resource_tags: dep.resource_tags.clone(),
        })
    }).collect();
    (
        wi.title.clone(),
        wi.description.clone(),
        wi.phase_id.clone(),
        wi.acceptance_criteria.clone(),
        wi.resource_tags.clone(),
        deps,
    )
};

self.work_acceptance_criteria = wi_ac;
self.work_resource_tags = wi_rt;
self.dependency_summaries = dep_summaries;
```

This replaces the current 3-field extraction at `context.rs:299-303` (which only reads `title`, `description`, `phase_id`). A single guard scope ensures the Work and its dependencies are read from the same snapshot. No changes to `load_bundle_hierarchy` are needed - it delegates to `load_work_hierarchy` at line 414, so Reviewer context gets the enrichment automatically.

### Changes to build (hierarchy rendering)

The existing Work line rendering (`context.rs:522-524`) stays unchanged. Append the new sections immediately after it, before the `hierarchy.push('\n')` on line 525:

```rust
// --- existing code (unchanged) ---
if let Some((ref title, ref desc)) = self.work {
    hierarchy.push_str(&format!("**Work:** {} - {}\n", title, desc));
}

// --- new sections (insert here) ---

// Acceptance criteria
if !self.work_acceptance_criteria.is_empty() {
    hierarchy.push_str("\n**Acceptance Criteria:**\n");
    for ac in &self.work_acceptance_criteria {
        hierarchy.push_str(&format!("- {}\n", ac));
    }
}

// Allowed files
if !self.work_resource_tags.is_empty() {
    hierarchy.push_str("\n**Allowed Files:**\n");
    for tag in &self.work_resource_tags {
        hierarchy.push_str(&format!("- {}\n", tag));
    }
}

// Dependency outputs
if !self.dependency_summaries.is_empty() {
    hierarchy.push_str("\n**Dependencies:**\n");
    for dep in &self.dependency_summaries {
        let files = dep.resource_tags.join(", ");
        hierarchy.push_str(&format!(
            "- [{}] {} - files: {}\n",
            dep.status, dep.title, files
        ));
    }
}
// --- end new sections ---
```

### Rendered Output Example

```
## Hierarchy

**Plan:** Build Lua Todo App - CLI todo list app in Lua
**Spec:** Lua Todo CLI - Two-phase implementation
**Phase:** Phase 1 - Core implementation
**Work:** Create test_todo.lua - Write tests for the TodoStore

**Acceptance Criteria:**
- All tests pass when run with lua test_todo.lua
- Tests cover add, list, done, and delete operations

**Allowed Files:**
- test_todo.lua

**Dependencies:**
- [Done] Create todo.lua - files: todo.lua
```

The implementer now knows: (1) read `todo.lua` first to discover the actual API, (2) only write to `test_todo.lua`, (3) tests must pass with `lua test_todo.lua`.

### Token Budget Impact

Typical overhead per Work item:
- Acceptance criteria: 3-5 lines, ~30-50 tokens
- Resource tags: 1-3 lines, ~10-20 tokens
- Dependencies: 1-3 lines, ~20-40 tokens

Total: ~60-110 additional tokens. The Implementer hierarchy budget is 2000 tokens. Current usage (Plan + Spec + Phase + Work title/desc) is typically 200-400 tokens. This fits comfortably.

### Implementation Plan

All changes are in `src/agents/context.rs`:

1. Add `DependencySummary` struct (~5 lines)
2. Add three new fields to `ContextBuilder` struct (~3 lines)
3. Initialize fields in `new()` (~3 lines)
4. Enrich `load_work_hierarchy` extraction (~20 lines)
5. Enrich `build` hierarchy rendering (~20 lines)
6. Add unit tests (~30 lines)

Total: ~80 lines changed in one file.

## Alternatives Considered

### Alternative 1: Inject dependency file contents directly

- **Description:** Read the actual source files of completed dependencies and include them in the context
- **Pros:** Implementer sees exact API, no guesswork
- **Cons:** Blows token budget. A single dependency file (e.g., 200-line `todo.lua`) is ~800 tokens, leaving no room for learnings or iteration history
- **Why not chosen:** Resource_tags already tell the implementer which files to `read_file`. The implementer has a `read_file` action - it just needs to know *which* files to read.

### Alternative 2: Auto-extract API signatures from dependency code

- **Description:** Parse completed dependency files, extract function/method signatures, include as structured metadata
- **Pros:** Compact, precise API surface
- **Cons:** Language-dependent parsing, significant complexity, fragile for dynamic languages (Lua, Python)
- **Why not chosen:** Premature. The simple approach (tell the implementer what files exist) lets the LLM discover the API via read_file. If this proves insufficient, structured extraction can be added later.

### Alternative 3: Add dependency info to Learnings instead

- **Description:** When a Work completes, auto-generate a Learning with its API surface
- **Pros:** Reusable across agents and iterations
- **Cons:** Learnings are scope-filtered and budget-constrained separately. Dependency info is always relevant to the consuming Work - it belongs in the hierarchy, not in an optional learning that might be truncated.
- **Why not chosen:** Wrong abstraction. This is structural context, not a learned insight.

## Technical Considerations

### Dependencies

- No new crate dependencies
- Only touches `src/agents/context.rs`
- Uses existing `Stores` read methods (no new queries)

### Performance

- No additional lock acquisitions: dependency resolution happens within the existing `read_works()` guard scope
- Lock-snapshot pattern preserved: brief read lock, clone, release
- Negligible impact - dependency lists are typically 0-3 items

### Testing Strategy

- Unit test: build a ContextBuilder with Work that has acceptance_criteria, resource_tags, and dependencies; verify the assembled message contains all three sections
- E2E validation: re-run lua-todo target after the fix; verify implementer reads `todo.lua` before writing `test_todo.lua`

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Hierarchy section exceeds token budget for complex plans | Low | Low | Existing truncation logic applies; new sections are small |
| Dependency Work not found (deleted/abandoned) | Low | Low | filter_map already handles missing entries gracefully |
| Reviewer sees enriched data it doesn't need | Low | Low | Reviewer hierarchy budget is also 2000 tokens; extra ~100 tokens is negligible. Acceptance criteria in hierarchy actually helps Reviewer verify bundle claims. |
| Truncation drops new sections first | Low | Med | New sections sit at the tail of hierarchy and would be truncated before Plan/Spec/Phase/Work lines. Mitigated by the typical case being well within budget (260-510 of 2000 tokens). If needed, can reorder to put dependencies before Work description. |
| Implementer doesn't read dependency files despite seeing them | Med | Med | Follow-up: add guidance to implementer.pmt: "If your Work has dependencies, read their output files before writing." Context alone may not change behavior without prompt guidance. |
| Dependency with empty resource_tags | Low | Low | Renders as `- [Done] Title - files: ` (empty file list). Harmless - dependency title alone still provides context. |

## Follow-up Work

- Update `implementer.pmt` step 1 (Read) to explicitly say: "If your Work has dependencies, read their output files first to discover the API before writing."
- This is a one-line prompt change that reinforces the new context data.

## Open Questions

- [ ] Should dependency resource_tags be shown to the Coordinator when it generates Learnings about cross-work failures?
- [ ] If hierarchy truncation becomes a real issue, should dependencies render before the Work description line (since they're more critical for correctness)?

## References

- `src/agents/context.rs` - ContextBuilder implementation
- `src/domain/work.rs` - Work struct with dropped fields
- `prompts/implementer.pmt` - references resource_tags without receiving them
- `docs/design/2026-02-25-orchestration-spine.md` - daemon and FSM architecture
- `docs/design/2026-02-26-multi-level-rwl.md` - Coordinator and Implementer roles
