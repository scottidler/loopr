# Design Document: Domain Model Cleanup

**Author:** Scott A. Idler
**Date:** 2026-04-07
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Strip the Plan/Spec/Phase/Work domain model down to its true shape. Remove vestigial fields (`description`, `checklist`, `validation_commands`), rename `resource_tags` to `files`, add `parent_id` to Plan for structural uniformity, and add markdown links between parent and child docs. Every callsite, prompt, handler, test, CLI flag, and frontmatter key that touches a changed field must be updated. No vestiges.

## Problem Statement

### Background

The domain model accumulated fields during rapid iteration. The `description` field was the first to go (marked `skip_serializing` in v0.1.84), but its ghost remains on every struct and dozens of callsites still read it. `checklist` on Work was an experiment that never proved useful - AC already covers validation. `validation_commands` on Phase was replaced by AC and is already `skip_serializing`. `resource_tags` is a terrible name for what is just a list of file paths. Plan lacks `parent_id` while the other three have it, breaking uniformity.

Two supplemental docs map the territory:
- `docs/description-field-callsites.md` - every callsite that reads `.description`
- `docs/field-necessity-evaluation.md` - field-by-field necessity audit

### Problem

The model has dead fields, misnamed fields, and inconsistent structure. Every dead field is a trap for future code that wires into something that no longer persists. Every callsite reading `.description` gets an empty string after daemon restart and silently degrades prompt quality.

### Goals

1. Remove `description` field from Plan, Spec, Phase, Work structs entirely
2. Remove `checklist` and `ChecklistItem` from Work entirely
3. Remove `validation_commands` from Phase entirely
4. Rename `resource_tags` to `files` on Work (struct, JSONL, frontmatter, prompts, CLI, handlers, work queue, lock contention)
5. Add `parent_id: Option<String>` to Plan (always None), keeping `parent_id: String` on Spec/Phase/Work
6. Add markdown links from parent docs to child docs in `docs/loopr/`
7. Update every callsite, prompt template, IPC handler, CLI flag, test, and frontmatter key
8. `otto ci` passes after every phase

### Non-Goals

- Changing the persistence architecture (JSONL stays as gospel, .md stays as content)
- Introducing a shared `Doc` trait or generic `Doc<S>` struct (premature - do it when needed)
- Changing the execution model (specs/phases sequential, works parallel)
- Modifying the FSM or status enums
- Touching TaskStore internals
- Removing or reconciling the old `Doc` struct (`src/domain/doc.rs`) and `DocKind` enum used by the decomposer - that is a separate cleanup

### Exempt Fields

The following `description` and `resource_tags` fields are **not** part of this cleanup. They are semantically distinct from the Plan/Spec/Phase/Work fields being removed or renamed. A mechanical grep-and-remove must not touch them:

| Field | Type | Why exempt |
|-------|------|-----------|
| `Bundle.description` | `Option<String>` (`src/domain/bundle.rs`) | Bundle's own summary field, not document prose |
| `BundleCreateParams.description` | `Option<String>` (`src/ipc/params.rs`) | IPC param for Bundle, not domain docs |
| `LlmCoverageGap.description` | `String` (`src/evaluator.rs`) | LLM response deserialization - describes a coverage gap |
| `LlmOutOfScopeItem.description` | `String` (`src/evaluator.rs`) | LLM response deserialization - describes an out-of-scope item |
| `ChecklistItem.description` | `String` (`src/domain/work.rs`) | Goes away with Phase 2 (checklist removal) - do not conflate with Phase 1 |
| `ToolDefinition.description` | `String` (`src/tools/`) | Tool metadata, unrelated |
| `Learning.resource_tags` | renamed to `files` in Phase 4 | This IS part of the cleanup, but is Learning's own field, not Work's |

## Proposed Solution

### Overview

Four flat structs, each with the same core fields plus type-specific extras. No dead weight.

### Target Data Model

**Shared fields (all four types):**

| Field | Type | Persisted in |
|-------|------|-------------|
| `id` | `String` | JSONL + .md frontmatter |
| `parent_id` | `Option<String>` (Plan) / `String` (others) | JSONL + .md frontmatter |
| `title` | `String` | JSONL + .md frontmatter |
| `acceptance_criteria` | `AcceptanceCriteria` | JSONL + .md frontmatter |
| `status` | type-specific enum (private) | JSONL + .md frontmatter |
| `created_at` | `i64` | JSONL + .md frontmatter |
| `updated_at` | `i64` | JSONL + .md frontmatter |

**Type-specific fields:**

| Type | Field | Type | Purpose |
|------|-------|------|---------|
| Plan | `tier` | `Tier` | Brief vs Full routing |
| Spec | `order` | `u32` | Sequence within Plan |
| Phase | `order` | `u32` | Sequence within Spec |
| Work | `files` | `Vec<String>` | File paths scoping allowed changes |
| Work | `dependencies` | `Vec<String>` | Work IDs that must complete first |
| Work | `assignee` | `Option<String>` | Agent session owning the work |

**Removed fields:**

| Field | Was on | Reason |
|-------|--------|--------|
| `description` | all four | .md body IS the description |
| `checklist` | Work | AC covers validation; checklist was unused experiment |
| `ChecklistItem` | Work | Goes with checklist |
| `validation_commands` | Phase | Replaced by AC; already skip_serializing |
| `resource_tags` | Work | Renamed to `files` |

### Target Struct Shapes

```rust
// plan.rs
pub struct Plan {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,  // always None; exists for structural uniformity
    pub title: String,
    pub acceptance_criteria: AcceptanceCriteria,
    status: PlanStatus,
    pub tier: Tier,
    pub created_at: i64,
    pub updated_at: i64,
}

// spec.rs
pub struct Spec {
    pub id: String,
    pub parent_id: String,  // Plan ID
    pub title: String,
    pub acceptance_criteria: AcceptanceCriteria,
    status: SpecStatus,
    pub order: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

// phase.rs
pub struct Phase {
    pub id: String,
    pub parent_id: String,  // Spec ID
    pub title: String,
    pub acceptance_criteria: AcceptanceCriteria,
    status: PhaseStatus,
    pub order: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

// work.rs
pub struct Work {
    pub id: String,
    pub parent_id: String,  // Phase ID (Full) or Plan ID (Brief)
    pub title: String,
    pub acceptance_criteria: AcceptanceCriteria,
    status: WorkStatus,
    pub files: Vec<String>,
    pub dependencies: Vec<String>,
    pub assignee: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
```

**Note on parent_id types:** `parent_id` is `Option<String>` only on Plan (always None). On Spec/Phase/Work it stays `String` because it's never None on those types, and wrapping it in Option would add `.unwrap()` / `.as_deref()` noise at every callsite for zero benefit.

### Markdown File Structure

Each doc at `docs/loopr/<id>.md`:

```markdown
---
id: pl-abc123
parent-id: ~
title: Build Authentication System
status: Active
tier: Full
acceptance-criteria:
  - Users can log in with email and password
  - Sessions expire after 30 minutes
created-at: 2026-04-07T12:00:00Z
updated-at: 2026-04-07T12:05:00Z
children:
  - "[Auth Backend Spec](sp-def456.md)"
  - "[Auth Frontend Spec](sp-ghi789.md)"
---

[Full prose body - the content that was formerly `description`]

## Acceptance Criteria

- [ ] Users can log in with email and password
- [ ] Sessions expire after 30 minutes
```

The `children` key in frontmatter provides downward traversal. `parent-id` provides upward traversal. The full tree is navigable from any node.

### Callsite Changes

#### 1. Domain structs (src/domain/)

| File | Change |
|------|--------|
| `plan.rs` | Remove `description` field. Add `parent_id: Option<String>` (always None). Remove `description` from `new()`, serde, `doc_body()`, `doc_frontmatter()`. Add `parent-id` to frontmatter. Add `children` to frontmatter. |
| `spec.rs` | Remove `description` field. Remove `description` from `new()`, serde, `doc_body()`. Add `children` to frontmatter. `parent_id` stays `String`. |
| `phase.rs` | Remove `description` field. Remove `validation_commands` field. Remove both from `new()`, serde, `doc_body()`. Add `children` to frontmatter. `parent_id` stays `String`. |
| `work.rs` | Remove `description` field. Remove `checklist` field. Remove `ChecklistItem` struct. Rename `resource_tags` to `files`. Update `new()`, serde, `doc_body()`, `doc_frontmatter()`. Rename `resource-tags` to `files` in frontmatter. `parent_id` stays `String`. |

#### 2. Constructors

All `::new()` signatures lose the `description: String` parameter:

| Current | Target |
|---------|--------|
| `Plan::new(title, description, ac)` | `Plan::new(title, ac)` |
| `Spec::new(parent_id, title, description)` | `Spec::new(parent_id, title)` |
| `Phase::new(parent_id, title, description, order)` | `Phase::new(parent_id, title, order)` |
| `Work::new(parent_id, title, description)` | `Work::new(parent_id, title)` |

#### 3. DocMarkdown impls (src/domain/)

| Method | Change |
|--------|--------|
| `doc_body()` on all four | Stop reading `self.description`. After this change, `doc_body()` returns only the AC section (rendered as `## Acceptance Criteria` with checkbox list). The prose body is not on the struct - it is either passed explicitly to `write_doc_markdown_body()` during creation, or already on disk and preserved by `write_doc_markdown()` during frontmatter-only updates. |
| `doc_frontmatter()` on Plan | Add `parent-id: ~`, add `children` list |
| `doc_frontmatter()` on Spec, Phase | Add `children` list |
| `doc_frontmatter()` on Work | Rename `resource-tags` to `files`, remove checklist rendering |

#### 4. Decomposer (src/decomposer/)

| Change |
|--------|
| `ChildRecord` and `ChildEntry` still carry the LLM prose content (the markdown body) - this is needed to write the .md file. But the content goes directly to `write_doc_markdown_body()` instead of being stored on the struct's `.description` field. |
| `records_to_hierarchy()`: stop setting `.description` on Plan/Spec/Phase/Work. The struct no longer has the field. |
| `persist_hierarchy()`: when writing each record, call `write_doc_markdown_body(repo_path, &record, &content)` with the prose from `ChildRecord`. |
| Rename `resource_tags` to `files` on any Work-related decomposer output (currently empty from decomposer). |
| Stop initializing `checklist` and `validation_commands` - fields no longer exist. |

#### 5. IPC Handlers (src/daemon/handlers/)

| File | Change |
|------|--------|
| `doc.rs` | `accept_plan_markdown()`: stop extracting description into struct field. Pass prose body directly to `write_doc_markdown_body()`. `persist_hierarchy()`: after persisting each record, update parent's .md to include child link. |
| `plan.rs` | `handle_plan_create()`: remove `description` from params. `handle_plan_update()`: remove `description` from updatable fields - body updates go through `write_doc_markdown_body()` if needed. |
| `spec.rs` | `handle_spec_create()` if exists: remove `description` param |
| `phase.rs` | `handle_phase_create()` if exists: remove `description` param, remove `validation_commands` |
| `work.rs` | `handle_work_create()`: rename `resource_tags` to `files` in param parsing. Remove `description` param. Update validation message ("Work must have at least one file" instead of "resource_tag"). |

**Directive: `persist_one!` macro rewrite.** The current `persist_one!` macro in `doc.rs` hardcodes a call to `write_doc_markdown()`, which for new files falls back to `record.doc_body()`. Once `description` is removed, `doc_body()` returns only the rendered AC section - the LLM prose body is lost.

The `DecomposedHierarchy.content` map (id -> LLM prose) is currently dead data - populated by the decomposer but never read by `persist_hierarchy`. This must be fixed:

1. Remove or rewrite `persist_one!` to accept an optional body string
2. For each record, look up `hierarchy.content.get(&r.id)` to get the LLM prose
3. If content exists, call `write_doc_markdown_body(repo_path, &r, &content)` instead of `write_doc_markdown(repo_path, &r)`
4. If no content exists (shouldn't happen, but defensive), fall back to `write_doc_markdown(repo_path, &r)`

Without this change, every decomposed record gets a `.md` file with nothing but an AC checklist. Every subsequent `read_doc_content()` call returns just the AC section. The LLM gets acceptance criteria but zero context about what the work actually is. The system appears to work (no errors, no panics) but is silently lobotomized.

#### 6. Context Builder (src/agents/context/)

Every callsite that reads `.description` (lines 337-401 per callsite doc) must be replaced with `read_doc_content_or_empty(repo_path, &id)` to read the prose body from the .md file on disk.

| Current | Target |
|---------|--------|
| `work.description` | `read_doc_content_or_empty(repo_path, &work.id)` |
| `plan.description` | `read_doc_content_or_empty(repo_path, &plan.id)` |
| `phase.description` | `read_doc_content_or_empty(repo_path, &phase.id)` |
| `spec.description` | `read_doc_content_or_empty(repo_path, &spec.id)` |

The context builder has access to `self.stores.config.project.repo_path`. Use the `_or_empty` variant per the I/O Failure Contract - a missing `.md` file should degrade the prompt, not crash the context assembly.

Also: rename `resource_tags` references to `files` (the "Allowed Files" section).

#### 7. Generation (src/agents/generation/)

Lines 76-102 per callsite doc: replace `.description` reads with doc content. Rename `resource_tags` to `files` in prompt building.

**Directive: no I/O in prompt builders.** Functions in `generation.rs` (e.g., `build_work_generation_prompt`) are pure prompt-building functions that accept domain structs like `&Phase` and `&[Work]`. They do not have access to `repo_path` or `Stores` and must not be polluted with disk I/O.

The migration pattern is:
1. The **caller** (e.g., `src/agents/coordinator.rs`) performs `read_doc_content(repo_path, &id)` to read the prose body from disk
2. The caller passes the resulting `String` into the generator function as a new `&str` parameter (e.g., `phase_content: &str`, `work_contents: &[(&str, &str)]`)
3. The generator function uses the passed-in content string where it previously read `self.description`

Do not inject `repo_path`, `Stores`, or any I/O capability into generation functions. They remain pure string-in, string-out.

#### 8. Integrator (src/agents/integrator/)

Lines 308-505 per callsite doc: replace `.description` reads with `read_doc_content()` in validation and coverage evaluator calls.

The integrator handler has two categories of `.description` reads:

1. **Single-record reads** (validator calls at lines 308, 328, 348): replace with `read_doc_content(repo_path, &id)`. The handler has access to `stores.config.project.repo_path`.

2. **Loop reads** (summary building at lines 428, 464, 499): currently `format!("- [{}] {}: {}", s.id, s.title, s.description)` inside `.map()`. Replace with `read_doc_content()` per iteration. This is O(N) file reads but N is small (typically <30 records) and these are advisory summary strings, not hot paths.

All `read_doc_content()` calls in the integrator must use the error fallback contract (see Technical Considerations: I/O Failure Contract).

#### 9. Evaluator (src/evaluator/)

Line 110: `PhaseWorksParams` - replace `.description` with doc content read. Rename any `resource_tags` references.

**Directive:** Rename `PhaseWorksParams.description` to `PhaseWorksParams.content`. The caller (`src/daemon/handlers/integrator.rs`) reads the prose body from disk via `read_doc_content()` and passes it into this field. The evaluator itself performs no I/O - it receives content as a string.

Note: `LlmCoverageGap.description` and `LlmOutOfScopeItem.description` are **exempt** - these are LLM response deserialization fields, not domain document descriptions (see Exempt Fields).

#### 10. Work Queue (src/daemon/work_queue.rs)

Rename `work.resource_tags` to `work.files` in:
- `compute_priority()` contention check (line 77)
- All test fixtures that set `resource_tags`

#### 11. CLI (src/cli.rs, src/cli/dispatch.rs)

| Current | Target |
|---------|--------|
| `--resource-tag` flag | `--file` flag |
| `resource_tags` field in CrudCmd | `files` field |
| `description` field in CrudCmd | Remove entirely |
| dispatch.rs `resource_tags` param | `files` param |

**Directive: CLI creation is scaffold-only.** Removing `description` from CrudCmd means `loopr plan create --title "..."` scaffolds a `.md` file with frontmatter and AC only - no prose body. This is the intended behavior. The prose-heavy ingress path is `doc.accept` (chat funnel) or `doc.inject` (file path), both of which provide the full markdown body.

If a user needs to set prose content from the CLI, they either:
1. Use `doc.inject --path plan.md` with a pre-written markdown file, OR
2. Create via CLI, then manually edit `docs/loopr/<id>.md`

Do not introduce a `--body-file` flag or a `--description` replacement. The CLI create path is for quick scaffolding; rich content comes through the doc pipeline.

#### 12. Prompt Templates (prompts/)

| File | Change |
|------|--------|
| `coordinator.pmt` | Rename `resource_tags` to `files` in create_work examples and guidance |
| `generation-work.pmt` | Rename `resource_tags` to `files` |
| `implementer.pmt` | Rename `resource_tags` to `files` ("files in your Work define your allowed files") |
| `researcher.pmt` | Rename `resource_tags` to `files` if referenced |
| `reviewer.pmt` | Rename `resource_tags` to `files` |
| `coverage-phase-works.pmt` | Rename `resource_tags` to `files` |
| `coverage-plan-specs.pmt` | Replace `{description}` placeholder with doc content |
| `coverage-spec-phases.pmt` | Replace `{description}` placeholder with doc content |
| `validator-plan.pmt` | Replace `{description}` placeholder |
| `validator-spec.pmt` | Replace `{description}` placeholder |
| `validator-phase.pmt` | Replace `{description}` placeholder |
| `decompose/work.pmt` | Rename `resource_tags` to `files` if present |

#### 13. Validator (src/validator/)

`prompts.rs`: `plan_prompt()`, `spec_prompt()`, `phase_prompt()` - these take `description` as a parameter and substitute `{description}`. Change to accept the doc body content instead (read from .md by the caller).

#### 14. Markdown Module (src/domain/markdown.rs)

| Function | Change |
|----------|--------|
| `write_doc_markdown()` | No longer calls `doc_body()` for description content. If body exists on disk, preserve it. Update frontmatter only. |
| `write_doc_markdown_body()` | Accepts explicit body string (from LLM). Writes frontmatter + body. |
| Add `update_parent_children()` | New function: reads parent .md, adds child link to `children` frontmatter list, writes back. |
| `read_doc_content()` | Already exists at `src/domain/markdown.rs:85`. No changes needed - callers just need to use it. |
| Add `read_doc_content_or_empty()` | New convenience function: calls `read_doc_content()`, logs warning on failure, returns empty string. All non-test callsites should use this instead of raw `read_doc_content()`. See I/O Failure Contract. |

#### 15. Adjacent types with `resource_tags` or `description`

These types are NOT Plan/Spec/Phase/Work but reference the same field names:

| Type | Field | Change |
|------|-------|--------|
| `Learning` (`src/domain/learning.rs`) | `resource_tags: Vec<String>` | Rename to `files` with `#[serde(alias = "resource_tags")]`. Learning uses these the same way Work does - scoping to file paths. |
| `Bundle` (`src/domain/bundle.rs`) | comment referencing "Work's resource_tags scope" | Update comment to say "Work's files scope" |
| `evaluator/prompts.rs` | `spec_phases_prompt(spec_description)` param | Rename param to `spec_body` or `spec_content`, caller reads from .md |
| `evaluator/prompts.rs` | similar for plan and phase prompt functions | Same pattern |
| `coverage.rs` params | `description: String` on evaluator param structs | Rename to `body` or `content`, populated from .md read |

#### 16. Tests

Every test that constructs Plan/Spec/Phase/Work with a `description` argument must be updated. Every test that reads `.description`, `.checklist`, `.validation_commands`, or `.resource_tags` must be updated.

Key test files:
- `src/domain/plan.rs` tests
- `src/domain/spec.rs` tests
- `src/domain/phase.rs` tests
- `src/domain/work.rs` tests
- `src/daemon/handlers.rs` tests (JSON fixtures with `resource_tags`)
- `src/daemon/handlers/worktree.rs` tests
- `src/daemon/work_queue.rs` tests
- `src/cli.rs` tests (`test_cli_parses_work_create_with_resource_tags`)
- `src/tests/fsm/` tests
- `src/tests/integration/` tests

#### 17. JSONL Backward Compatibility

Old JSONL records contain `description`, `checklist`, `validation_commands`, and `resource_tags`. For deserialization:

| Field | Strategy |
|-------|----------|
| `description` | Add `#[serde(default, skip_serializing, rename = "description")]` as a dead field temporarily, or just rely on serde's `deny_unknown_fields` not being set (default: ignore unknown). Since we currently DON'T use deny_unknown_fields, old JSONL with `description` will simply be ignored on deserialization. Confirm this. |
| `checklist` | Same - serde ignores unknown fields by default. |
| `validation_commands` | Same. |
| `resource_tags` | Use `#[serde(alias = "resource_tags")]` on the `files` field so old JSONL records deserialize correctly. |

### Implementation Plan

Each phase is independently shippable. Run `otto ci` after each.

#### Phase 0: Verify serde ignores unknown fields

Before removing anything, write a test that confirms deserializing a JSON blob with extra fields (description, checklist, validation_commands) into the target struct silently ignores them. This is the safety net for JSONL backward compat.

#### Phase 1: Remove `description` from all four structs

1. Remove the field from Plan, Spec, Phase, Work structs
2. Remove from all `::new()` constructors
3. Update every callsite that sets `.description` (decomposer, handlers) - **check against Exempt Fields list to avoid removing Bundle.description, LLM struct fields, etc.**
4. Update every callsite that reads `.description`:
   - Context builder, integrator handler, evaluator: call `read_doc_content_or_empty()` (see I/O Failure Contract)
   - Generation functions: **caller pre-reads from disk and passes content as `&str`** (see Section 7 directive). Do not inject I/O into generation functions.
5. Update `doc_body()` impls to not reference `self.description` - returns only rendered AC section
6. **Rewrite `persist_one!` macro** in `doc.rs` to use `hierarchy.content` map and call `write_doc_markdown_body()` (see Section 5 directive). This is the highest-risk step - without it, all .md files get empty bodies.
7. Update validator prompt functions to accept body content from caller
8. Update all prompt templates that use `{description}` placeholder
9. Update evaluator prompt functions and param structs: rename `description` to `content` (see Section 9 directive)
10. Update all tests - **remove `description` from JSON fixtures, rewrite assertions** (see Testing Strategy directive on zombie fixtures)
11. `otto ci`

#### Phase 2: Remove `checklist` and `ChecklistItem` from Work

1. Remove `checklist: Vec<ChecklistItem>` from Work struct
2. Remove `ChecklistItem` struct
3. Remove checklist rendering from `doc_body()` and `doc_frontmatter()`
4. Update tests
5. `otto ci`

#### Phase 3: Remove `validation_commands` from Phase

1. Remove `validation_commands: Vec<String>` from Phase struct
2. Update `Phase::new()` to not initialize it
3. Update tests
4. `otto ci`

#### Phase 4: Rename `resource_tags` to `files`

1. Rename field on Work struct: `resource_tags` -> `files`
2. Add `#[serde(alias = "resource_tags")]` for JSONL backward compat
3. Rename in `doc_frontmatter()`: `resource-tags` -> `files`
4. Rename CLI flag: `--resource-tag` -> `--file`
5. Rename in all IPC param parsing (handlers, dispatch)
6. Rename in work_queue.rs contention check
7. Rename in context builder ("Allowed Files" section)
8. Rename in all prompt templates (.pmt files)
9. Rename in all tests
10. Rename in `indexed_fields()` if indexed
11. Rename `resource_tags` to `files` on `Learning` struct (`src/domain/learning.rs`) with `#[serde(alias = "resource_tags")]`
12. Update `Bundle` comment referencing "resource_tags scope" to "files scope"
13. `otto ci`

#### Phase 5: Add `parent_id: Option<String>` to Plan

1. Add `parent_id: Option<String>` to Plan struct (always None)
2. Add `#[serde(default)]` for backward compat with old JSONL that lacks this field
3. Update `Plan::new()` to set `parent_id: None`
4. Add `parent-id` to Plan's `doc_frontmatter()` (render as `~` for None)
5. Spec/Phase/Work keep `parent_id: String` unchanged - no Option wrapping
6. Update tests
7. `otto ci`

#### Phase 6: Add `order` to Spec and enforce sequential spec/phase execution

1. Add `order: u32` to Spec struct
2. Add `#[serde(default)]` for backward compat with old JSONL that lacks the field (defaults to 0)
3. Update `Spec::new()` to accept `order` parameter
4. Update decomposer to assign sequential order values to specs (0, 1, 2...)
5. Add `order` to Spec's `doc_frontmatter()`
6. Rewrite `find_next_phase_to_activate()` in coordinator.rs (see algorithm below)
7. Update CLI if spec creation accepts order
8. Update tests
9. `otto ci`

**Directive: decomposer spec ordering.** The decomposer (`src/decomposer.rs`) currently tracks `phase_counters: HashMap<String, u32>` to assign sequential `order` values to phases within each spec. Add an identical `spec_counter: u32` (not a HashMap - specs are all children of the same plan) that increments for each spec record:

```rust
let mut spec_counter: u32 = 0;
// ...
DocKind::Spec => {
    let mut spec = Spec::new(parent_id, child.title.clone(), spec_counter);
    spec_counter += 1;
    // ...
}
```

Specs are processed in the order they appear in `all_records` after filtering by `DocKind::Spec`. This matches the LLM output order, which is the intended decomposition order.

**Directive: `find_next_phase_to_activate()` algorithm.** The current implementation at `coordinator.rs:841-863` does not enforce spec boundaries. Rewrite to this exact algorithm:

```
fn find_next_phase_to_activate(stores, plan_id) -> Option<Phase>:
    1. Read all specs where parent_id == plan_id
    2. Sort specs by spec.order ascending
    3. For each spec in order:
        a. Read all phases where parent_id == spec.id
        b. Sort phases by phase.order ascending
        c. If any phase in this spec is non-terminal (not Complete, not Abandoned):
            - Return the first phase that is Draft (needs activation)
            - If no Draft phases but some are still Active/in-progress, return None
              (current phase still running, wait)
        d. If ALL phases in this spec are terminal: continue to next spec
    4. If all specs exhausted: return None (plan is done)
```

The critical invariant: **never look at phases from Spec N+1 while Spec N has non-terminal phases.** The current code violates this by iterating across all specs and returning the first unfinished phase it finds regardless of spec boundaries.

#### Phase 7: Add markdown links (children in frontmatter)

1. Add `children` rendering to `doc_frontmatter()` for Plan, Spec, Phase
2. Implement `update_parent_children()` in markdown.rs
3. Call it from `persist_hierarchy()` after each child is created
4. Call it from `handle_work_create()` after work is created
5. Update tests
6. `otto ci`

## Alternatives Considered

### Alternative 1: Shared Doc<S> generic struct
- **Description:** Extract shared fields into `Doc<S>` parameterized by status type, compose with type-specific extensions
- **Pros:** No field duplication across four structs
- **Cons:** Type parameter threading through every function signature and trait impl. Serde complexity. Over-engineering for seven shared fields.
- **Why not chosen:** Premature abstraction. The four structs are simple and stable. Add the generic when field duplication actually causes a bug or maintenance burden, not before.

### Alternative 2: Doc trait for shared interface
- **Description:** Keep flat structs but add a `Doc` trait with accessors for shared fields
- **Pros:** Polymorphic functions over any doc type
- **Cons:** Trait impls are boilerplate for seven fields across four types
- **Why not chosen:** No current code needs to operate on "any doc type" polymorphically. The few places that do (markdown rendering, Record trait) already have their own traits. Add when needed.

### Alternative 3: Remove `order` and use dependencies for sequencing
- **Description:** Express Spec/Phase ordering as dependency chains instead of a u32
- **Pros:** Unified mechanism with Work dependencies
- **Cons:** Specs and Phases are sequential by definition. A dependency graph that's always a linked list is unnecessary complexity. Display order still needs a sort key.
- **Why not chosen:** `order` directly expresses what we mean (these run in this sequence). Dependencies pretend there's a graph when there isn't one.

## Technical Considerations

### Dependencies

- TaskStore (`scottidler/taskstore`): no changes needed. It's generic over Record trait. Struct field changes are transparent to it.
- `serde`: must confirm unknown-field-ignoring behavior for JSONL backward compat
- `loopr-derive` (Fsm macro): no changes - operates on status enums, not struct fields

### I/O Failure Contract for `read_doc_content()`

Replacing a memory read (`self.description`) with a disk read (`read_doc_content()`) introduces fallibility. Every callsite must handle the error case identically:

**Contract:** If `read_doc_content()` fails (file not found, corrupted, permissions), **log a warning and return an empty string.** Do not crash the daemon, do not propagate the error, do not skip the LLM call.

```rust
let content = read_doc_content(&repo_path, &id).unwrap_or_else(|e| {
    tracing::warn!("read_doc_content failed for {}: {}", id, e);
    String::new()
});
```

This matches the advisory pattern already used by `write_doc_markdown()` ("failure logs a warning but MUST NOT propagate to the caller").

**Rationale:** The `.md` files are derived state (written from in-memory data during persist). They can be missing in several legitimate scenarios:
- Daemon restart against a fresh or cleaned target repo where `docs/loopr/` was deleted
- Race condition between JSONL persist and `.md` write (crash between step 3 and step 4 of persist_one!)
- Records created via IPC handlers that have not yet had `write_doc_markdown()` called

In all these cases, degrading gracefully (empty content) is better than crashing the daemon. The LLM will produce lower-quality output but the system stays operational.

**Helper:** Consider adding a convenience function to `markdown.rs`:

```rust
pub fn read_doc_content_or_empty(repo_path: &Path, id: &str) -> String {
    read_doc_content(repo_path, id).unwrap_or_else(|e| {
        tracing::warn!("read_doc_content failed for {}: {}", id, e);
        String::new()
    })
}
```

All callsites outside of tests should use this helper instead of raw `read_doc_content()`.

### Performance

No impact. Removing fields makes structs smaller. Reading doc content from disk is already the pattern. The O(N) summary loops in the integrator handler (lines 428, 464, 499) perform one file read per child record, but N is typically small (<30) and these are advisory prompts, not hot paths.

### Testing Strategy

- Phase 0 establishes the backward compat safety net
- Each subsequent phase must pass `otto ci` before proceeding
- Existing FSM tests are unaffected (they test status transitions, not struct fields)
- Serde roundtrip tests must be updated for new field sets
- Test fixtures in handlers.rs, work_queue.rs, worktree.rs must update field names

**Directive: zombie test fixtures.** Dozens of integration tests in `src/tests/integration/`, `src/tests/fsm/`, and handler test modules inject raw `json!({"description": "..."})` payloads. After removing `description` from the structs, serde will silently ignore the `description` key in these JSON blobs (serde's default behavior - no `deny_unknown_fields`). The tests will pass but test nothing meaningful.

Two actions are required:

1. **Remove `description` from all JSON fixtures** - do not leave it as dead data that serde silently swallows. A fixture with a `description` key that does nothing is a lie about what the test covers.

2. **Replace prompt-content assertions with disk-read assertions** - any test that previously asserted `plan.description == "expected text"` must be rewritten to either:
   - Write a `.md` file to a `TestDir`, call the code under test, and assert `read_doc_content()` returns the expected content, OR
   - Assert that the prompt string passed to the LLM client contains the expected content (for context builder / generation tests)

Tests that just verify struct construction (`test_plan_new`, `test_work_new`) should remove the `description` parameter and stop asserting on it. Tests that verify IPC roundtrips should remove `description` from the JSON payload.

Key test files requiring non-trivial migration (not just parameter removal):
- `src/agents/context/tests.rs` - prompt content assertions
- `src/agents/coordinator/tests.rs` - hierarchy construction
- `src/daemon/handlers/work.rs` tests - JSON fixtures with `resource_tags`
- `src/daemon/handlers/bundle/tests.rs` - JSON fixtures
- `src/tests/integration/` - end-to-end payloads
- `src/decomposer.rs` tests - `test_records_to_hierarchy_*` assertions

### Backward Compatibility

JSONL files from before this change contain `description`, `checklist`, `validation_commands`, and `resource_tags`. Strategy:

1. Serde's default behavior ignores unknown fields - `description`, `checklist`, `validation_commands` silently dropped on deserialization
2. `#[serde(alias = "resource_tags")]` on the `files` field handles the rename
3. `#[serde(default)]` on `parent_id` handles old Plans that lack the field
4. No migration script needed - old data deserializes cleanly into new structs

### Execution Model

The scheduling rule is simple: **one phase active at a time, all its works in parallel.**

#### Sequential boundaries

Specs within a Plan execute sequentially by `order`. Phases within a Spec execute sequentially by `order`. The Coordinator must not activate Spec N until Spec N-1 is Complete. It must not activate Phase N until Phase N-1 is Complete. No work from a later spec or phase is ever scheduled while the current phase still has non-terminal works.

This means the entire system has exactly one active phase at any given time. All scheduling decisions happen within that phase.

#### Parallel within a phase

Within the active phase, all works are eligible for parallel execution. The work queue hands out every Ready work to available Implementer agents simultaneously, subject to two constraints:

1. **Dependencies** - a work cannot enter Ready until every work ID in its `dependencies` list is Done
2. **File contention** - the work queue deprioritizes (not blocks) works whose `files` overlap with active locks held by in-progress works

Without dependencies, all works in a phase start at the same time. With dependencies, the dependency graph within the phase governs ordering. The LLM specifies these relationships during decomposition.

#### What changes from current code

This is NOT just documenting existing behavior. Two things change:

**1. Spec has no `order` field today.** This design adds `order: u32` to Spec. Currently `find_next_phase_to_activate()` (coordinator.rs:841) iterates across ALL specs via `find_active_specs_for_plan()` which filters by `status == Active` but does not sort by order - because there is no order to sort by. The decomposer must assign `order` to specs at creation time, and the Coordinator must advance specs in order.

**2. `find_next_phase_to_activate()` does not enforce spec boundaries.** Today it walks all specs, then all their phases, and returns the first non-completed phase it finds. It does not check whether the current spec's phases are all done before moving to the next spec's phases. This must be fixed to: (a) find the current spec (lowest-order spec with non-terminal phases), (b) find the next phase within that spec, (c) only advance to the next spec when all phases in the current spec are Complete.

The work queue already filters by `current_phase_id` (work_queue.rs:35), so work scheduling within a phase is already correct. The dependency and file contention checks are also already correct.

#### Enforcement points

| Rule | Where enforced | Status |
|------|---------------|--------|
| Don't activate Spec N until Spec N-1 Complete | Coordinator: `find_next_phase_to_activate()` must respect spec boundaries | **Needs fix** |
| Don't activate Phase N until Phase N-1 Complete | Coordinator: `find_next_phase_to_activate()` returns phases in order, skipping completed ones | Already works |
| Don't schedule work from inactive phases | Work queue: `next_assignable_work()` filters by `parent_id == current_phase_id` | Already works |
| Don't schedule work with unmet dependencies | Work queue: checks all dependency IDs have `status == Done` | Already works |
| Deprioritize work with file contention | Work queue: `compute_priority()` scores -100 for works whose `files` overlap active locks | Already works (rename pending) |

#### Code changes required

1. **Add `order: u32` to Spec** - new field, assigned by decomposer at creation time
2. **Decomposer** - assign sequential `order` values to specs (0, 1, 2...) just as it does for phases
3. **`find_next_phase_to_activate()`** - rewrite to: sort specs by `order`, find the first spec with non-terminal phases, return the first non-completed phase within that spec. Do not cross spec boundaries.
4. **Spec `doc_frontmatter()`** - include `order` in frontmatter
5. **Spec `indexed_fields()`** - optionally index `order` for query efficiency

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Old JSONL fails to deserialize after field removal | Low | High | Phase 0 test proves serde ignores unknown fields before any removal |
| Callsite missed; reads empty string silently | Medium | Medium | Grep for every removed/renamed field name across entire codebase after each phase |
| Prompt templates reference old field names | Medium | Medium | Grep .pmt files for resource_tags, description, checklist; update all |
| Children links in frontmatter break on concurrent writes | Low | Low | Children are added during persist_hierarchy which holds the store lock |
| `persist_one!` macro not rewritten; .md files get empty bodies | High | Critical | persist_one! must use `hierarchy.content` map and call `write_doc_markdown_body()`. Without this, every decomposed record's .md file contains only AC - the LLM prose is silently discarded. System appears to work but all LLM calls get zero context. |
| Exempt field accidentally removed (Bundle.description, LLM structs) | Medium | High | Exempt Fields section documents every field that must survive. Review each `description` removal against the exempt list. |
| Daemon restart against repo without `docs/loopr/` directory | Medium | Medium | I/O failure contract: `read_doc_content_or_empty()` logs warning, returns empty string. System degrades gracefully. |
| Zombie test fixtures pass with dead assertions | High | Medium | Remove `description` from JSON fixtures (don't rely on serde ignoring it). Rewrite assertions to test disk-read behavior. |

## Open Questions

- [x] ~~Should the CLI `description` parameter be replaced with `--body-file <path>` for providing prose content, or should all prose come through the markdown acceptance pipeline (doc.accept)?~~ Resolved: CLI creation is scaffold-only (frontmatter + AC). Prose content comes through `doc.accept` (chat funnel) or `doc.inject` (file path). No `--body-file` flag. See Section 11 directive.
- [ ] Should `children` in frontmatter be rendered as markdown links `[title](id.md)` or bare IDs `["sp-abc123"]`? Links are human-navigable; bare IDs are simpler to parse.
- [ ] **Post-migration:** Prompt templates (.pmt files) and generation functions need a holistic rework of how document body content is presented to the LLM. The migration mechanically renamed `{description}` to `{content}` and labels from "Description:" to "Body:", but the templates were designed around a one-liner description field, not a full markdown document body read from disk. The framing, labels, and structure of these prompt sections should be redesigned to properly present document content.
- [x] ~~Should `parent_id` be `Option<String>` or sentinel?~~ Resolved: `Option<String>` on Plan only (always None). Spec/Phase/Work keep `String` because the value is never absent on those types.

## References

- `docs/description-field-callsites.md` - callsite map for description field
- `docs/field-necessity-evaluation.md` - field necessity audit
- `docs/design/2026-04-07-rip-out-staging-and-description.md` - predecessor design doc (implemented)
- `docs/templates/plan.md`, `spec.md`, `phase.md`, `work.md` - document templates
- `docs/templates/sections.yml` - section configuration
