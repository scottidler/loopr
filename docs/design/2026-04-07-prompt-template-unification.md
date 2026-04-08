# Design Document: Prompt Template Unification

**Author:** Scott A. Idler
**Date:** 2026-04-07
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Replace all per-field interpolation in `.pmt` files with a single `{markdown_content}` placeholder per document entity. The `docs/loopr/*.md` files already contain everything - title, status, order, AC in frontmatter, prose in body. Decomposing them into `{title}` + `{content}` + `{order}` + `{acceptance_criteria}` is redundant reassembly of what the LLM can read directly. This cleanup unifies the document-to-prompt interface, collapses prompt builder function signatures, and establishes a contract between `.pmt` files and Rust code.

## Problem Statement

### Background

The domain model cleanup (phases 1-7) moved all document content into `docs/loopr/<id>.md` files with YAML frontmatter. The `description` field was removed from all four domain types. But the `.pmt` files and their Rust prompt builders still decompose documents into individual fields:

```
# What we do now (validator-plan.pmt)
Title: {title}
Body: {content}
Acceptance Criteria: {acceptance_criteria}

# What the .md file already contains
---
id: pl-abc123
title: Build Authentication System
status: Active
acceptance-criteria:
  - Users can log in
  - Sessions expire after 30 minutes
children:
  - "[Auth Backend](sp-def456.md)"
---

[Full prose body here]

## Acceptance Criteria
- [ ] Users can log in
- [ ] Sessions expire after 30 minutes
```

The `.pmt` files pull apart what the `.md` file already has assembled. The Rust code reads the `.md` file, extracts individual fields, then stitches them back into the prompt. This is pointless.

### Problem

1. **Redundancy** - every document field appears in three places: the struct, the `.md` frontmatter, and the `.pmt` placeholder. Changes require updating all three.
2. **Stale placeholders** - `{description}` was removed from structs but references remain in `generation-work.pmt` and `coordinator.pmt`. `{resource_tags}` was renamed to `files` but may have residual references. No compiler catches these.
3. **Signature bloat** - prompt builder functions accept 4-6 parameters that are all extracted from the same `.md` file: `plan_prompt(title, content, acceptance_criteria)`, `phase_works_prompt(phase_title, phase_content, phase_order, spec_title, works_list)`.
4. **Untyped boundary** - the `.pmt` <-> Rust interface is string-based with no compile-time verification. A stale placeholder silently produces garbage prompts.

### Goals

1. Replace all document-field placeholders (`{title}`, `{content}`, `{order}`, `{acceptance_criteria}`, `{plan_title}`, `{spec_title}`, `{phase_title}`, `{plan_content}`, `{spec_content}`, `{phase_content}`, `{plan_acceptance_criteria}`, `{phase_order}`) with `{markdown_content}` (the full `.md` file)
2. For prompts needing parent or sibling context, use typed variants: `{parent_markdown_content}`, `{children_markdown_content_content}`
3. Collapse prompt builder function signatures to accept `&str` (the `.md` content)
4. Remove stale `description` references from `coordinator.pmt` and `generation-work.pmt`
5. Establish a placeholder registry: one place that lists every `{placeholder}` and its Rust source
6. `otto ci` passes after every phase

### Non-Goals

- Changing the `.md` file format or frontmatter structure
- Adding compile-time verification of `.pmt` placeholders (future work - noted as gap)
- Modifying the decompose/*.pmt templates (they already pass full markdown content)
- Changing the chat/*.pmt or interview.pmt templates (they don't reference documents)
- Modifying LLM response schemas (`coverage-schema.pmt`, `validator-schema.pmt`)

## Proposed Solution

### Overview

One document, one placeholder. The LLM receives the complete `.md` file and parses what it needs.

### Placeholder Taxonomy (after cleanup)

**Document content placeholders** - the `.md` file body:

| Placeholder | Meaning | Used in |
|---|---|---|
| `{markdown_content}` | The entity being evaluated/generated from | validator (plan), decompose |
| `{parent_markdown_content}` | The parent entity for context | validator (spec, phase), coverage evaluator |
| `{children_markdown_content_content}` | Concatenated child `.md` contents | validator (plan), coverage evaluator |

**Non-document placeholders** (unchanged):

| Placeholder | Source | Used in |
|---|---|---|
| `{schema}` | Embedded JSON schema `.pmt` | validator, coverage |
| `{query}` | Caller-provided search query | researcher |
| `{goal}` | User's goal text | interview, clarity |
| `{interview_exchanges}` | Accumulated Q&A | interview |
| `{diagnostic_section}` | System diagnostics | interview |
| `{work_status_values}` | Enum variant names | coordinator |
| `{bundle_status_values}` | Enum variant names | coordinator |
| `{hierarchy_status_values}` | Enum variant names | coordinator |
| `{work_override_statuses}` | Hardcoded list | coordinator |

### File-by-File Changes

#### Validator templates

**`validator-plan.pmt`** before:
```
Title: {title}
Body:
{content}
Acceptance Criteria:
{acceptance_criteria}
```

After:
```
## Document Under Review

{markdown_content}
```

The LLM reads title, AC, and body from the `.md` file directly.

**`validator-spec.pmt`** before:
```
Title: {title}
Body:
{content}
Parent Plan: {plan_title}
```

After:
```
## Document Under Review

{markdown_content}
```

The spec's frontmatter has `parent-id`, and the evaluation criteria reference the parent plan. Include the parent `.md` so the validator can check coherence:
```
## Parent Context

{parent_markdown_content}
```

**`validator-phase.pmt`** - same pattern. `{markdown_content}` for the phase, `{parent_markdown_content}` for the parent spec. The validator checks ordering correctness and dependency coherence against the parent.

#### Coverage templates

**`coverage-plan-specs.pmt`** before:
```
## Parent Plan
- Title: {plan_title}
- Body: {plan_content}
- Acceptance Criteria: {plan_acceptance_criteria}

## Generated Specs
{specs_list}
```

After:
```
## Parent Plan

{parent_markdown_content}

## Generated Specs

{children_markdown_content_content}
```

`{children_markdown_content_content}` is each spec's full `.md` content, separated by `---`.

**`coverage-spec-phases.pmt`** and **`coverage-phase-works.pmt`** - same pattern. Replace individual field placeholders with `{parent_markdown_content}` + `{children_markdown_content_content}`.

#### Coordinator template

**`coordinator.pmt`** - remove `"description": "..."` from the `create_work` JSON example. The `description` field no longer exists on Work. The coordinator creates Works with `title`, `files`, `acceptance_criteria`, `dependencies`.

#### Generation template

**`generation-work.pmt`** - rewrite prose references to "Phase's description" and "descriptions" to reference "the Phase's markdown content" or just "the Phase". Remove instruction about extracting acceptance_criteria from description - AC is in the frontmatter.

### Rust Code Changes

#### Validator prompt builders (`src/validator/prompts.rs`)

Before:
```rust
pub fn plan_prompt(title: &str, content: &str, acceptance_criteria: &str) -> String {
    store().validator_plan
        .replace("{title}", title)
        .replace("{content}", content)
        .replace("{acceptance_criteria}", acceptance_criteria)
        .replace("{schema}", &store().validator_schema)
}
```

After:
```rust
pub fn plan_prompt(markdown_content: &str) -> String {
    store().validator_plan
        .replace("{markdown_content}", markdown_content)
        .replace("{schema}", &store().validator_schema)
}
```

For spec/phase validators that need parent context:
```rust
pub fn spec_prompt(markdown_content: &str, parent_markdown_content: &str) -> String {
    store().validator_spec
        .replace("{markdown_content}", markdown_content)
        .replace("{parent_markdown_content}", parent_markdown_content)
        .replace("{schema}", &store().validator_schema)
}
```

#### Coverage evaluator prompt builders (`src/evaluator/prompts.rs`)

Before: 4-6 parameters per function.

After:
```rust
pub fn plan_specs_prompt(parent_markdown_content: &str, children_markdown_content: &str) -> String {
    store().coverage_plan_specs
        .replace("{parent_markdown_content}", parent_markdown_content)
        .replace("{children_markdown_content_content}", children_markdown_content)
        .replace("{schema}", &store().coverage_schema)
}
```

Same for `spec_phases_prompt` and `phase_works_prompt`.

#### Coverage evaluator callers (`src/evaluator.rs`, `src/daemon/handlers/integrator.rs`)

The callers currently read individual fields from `.md` files and struct fields, then pass them as separate arguments. After: read the full `.md` file once, pass it as `parent_markdown_content`. Build `children_markdown_content` by concatenating child `.md` files.

#### Generation callers (`src/agents/generation.rs`, `src/agents/coordinator.rs`)

`build_work_prompt` currently takes `phase_content: &str` and various metadata. Simplify to take the phase's full `.md` content. The LLM reads the frontmatter (order, AC, parent-id) directly.

### Implementation Plan

#### Phase 0: Domain model cleanup carryover fixes

Three items from `docs/design/2026-04-07-domain-model-cleanup.md` were not completed during that design doc's execution. They are included here rather than reopening the previous doc. These must be fixed first because the prompt unification depends on `.md` files having correct content.

**1. CRITICAL: Rewrite `persist_one!` to use `hierarchy.content` map**

`persist_one!` in `src/daemon/handlers/doc.rs` calls `write_doc_markdown(&repo_path, &r)`, which for new files falls back to `r.doc_body()`. Since `description` was removed, `doc_body()` returns only the AC checklist. The decomposer populates `hierarchy.content` (a `HashMap<String, String>` mapping id -> LLM prose), but `persist_hierarchy` never reads it. Every decomposed record gets a `.md` file with no prose body. The design doc explicitly warned this would "silently lobotomize" the system.

Fix: rewrite the persist loop to look up `hierarchy.content.get(&r.id)` for each record. If content exists, call `write_doc_markdown_body(&repo_path, &r, &content)`. If not, fall back to `write_doc_markdown(&repo_path, &r)`.

**2. Remove `description` from `CrudCmd::Create`**

`src/cli.rs` still has `description: String` on `CrudCmd::Create`. `src/cli/dispatch.rs` still maps it into IPC params. Remove the field, the CLI flag, and the dispatch mapping. The CLI create path is for quick scaffolding - rich content comes through the doc pipeline (`doc.accept` / `doc.inject`).

**3. Remove zombie test fixtures**

Tests in `src/daemon/handlers/plan.rs` (and potentially other handler test files) still inject `"description": "..."` in JSON payloads. Serde silently ignores the unknown field. These fixtures pass but test nothing meaningful. Remove `description` from all JSON test fixtures.

Steps:
1. Rewrite `persist_hierarchy` to use `hierarchy.content` map with `write_doc_markdown_body`
2. Remove `description` from `CrudCmd::Create` and `crud_to_ipc`
3. Remove `description` from all handler test JSON fixtures
4. Update dispatch tests that assert on `params["description"]`
5. `otto ci`

#### Phase 1: Foundation and validator templates

1. Add `read_full_markdown()` and `read_full_markdown_or_empty()` to `src/domain/markdown.rs`
2. Rewrite `validator-plan.pmt`, `validator-spec.pmt`, `validator-phase.pmt` to use `{markdown_content}` and `{parent_markdown_content}`
3. Collapse `src/validator/prompts.rs` function signatures to `(markdown_content: &str)` and `(markdown_content: &str, parent_markdown_content: &str)`
4. Update callers in `src/daemon/handlers/integrator.rs` to use `read_full_markdown_or_empty()`
5. Update tests
6. `otto ci`

#### Phase 2: Coverage templates

1. Rewrite `coverage-plan-specs.pmt`, `coverage-spec-phases.pmt`, `coverage-phase-works.pmt` to use `{parent_markdown_content}` + `{children_markdown_content_content}`
2. Add `build_children_markdown_content(repo_path, child_ids) -> String` helper to `src/domain/markdown.rs` - reads each child's full `.md`, joins with `---` separator
3. Collapse `src/evaluator/prompts.rs` function signatures to `(parent_markdown_content: &str, children_markdown_content: &str)`
4. Update callers in `src/evaluator.rs` and `src/daemon/handlers/integrator.rs` to use `read_full_markdown_or_empty()` + `build_children_markdown_content()`
5. Update tests
6. `otto ci`

#### Phase 3: Coordinator and generation templates

1. Remove `"description"` from `create_work` example in `coordinator.pmt`
2. Rewrite `generation-work.pmt` to remove "description" references
3. Update `src/agents/generation.rs` `build_work_prompt` to pass full `.md` content
4. Update coordinator callers
5. Update tests
6. `otto ci`

#### Phase 4: Dual placeholder validation

Every `.pmt` file gets two layers of validation. No exceptions.

**Layer 1: Static cross-reference script (`bin/check-pmt-placeholders`)**

A bash script that extracts `{placeholder}` patterns from all `.pmt` files and `.replace("{placeholder}"` targets from all Rust files, then diffs the two sets. Fails if:
- A `.pmt` file has a placeholder with no corresponding `.replace()` in Rust
- A Rust `.replace()` targets a placeholder that doesn't exist in any `.pmt` file

Add as a lint task in `.otto.yml`:
```yaml
- name: pmt-check
  run: bin/check-pmt-placeholders
```

**Layer 2: Rust residual-placeholder tests (`src/prompts.rs`)**

For every `.pmt` file, a Rust test that:
1. Calls the prompt builder function with sentinel values
2. Asserts zero `{...}` patterns remain in the output (regex: `\{[a-z_]+\}`)
3. Asserts every sentinel value appears in the output (proving the `.replace()` matched)

This exercises the actual runtime prompt assembly, not just static text matching.

Coverage required: every `.pmt` that uses `{...}` interpolation must have both checks. The full list:
- `validator-plan.pmt`, `validator-spec.pmt`, `validator-phase.pmt`
- `coverage-plan-specs.pmt`, `coverage-spec-phases.pmt`, `coverage-phase-works.pmt`
- `coordinator.pmt`
- `researcher.pmt`
- `interview.pmt`

Templates with no interpolation (`implementer.pmt`, `reviewer.pmt`, `chat*.pmt`, `decompose/*.pmt`, `tier-gate.pmt`) get the static check (Layer 1 confirms they have zero placeholders) but no Rust sentinel test (nothing to substitute).

Steps:
1. Write `bin/check-pmt-placeholders` script
2. Add `pmt-check` task to `.otto.yml`
3. Write Rust sentinel tests for every interpolated `.pmt` in `src/prompts.rs`
4. Grep for any remaining `{description}`, `{resource_tags}`, or other stale placeholders
5. `otto ci`

## Alternatives Considered

### Alternative 1: Keep individual field placeholders, just rename them

- **Description:** Rename `{content}` to `{markdown_content}` but keep `{title}`, `{order}`, etc.
- **Pros:** Smaller change. Explicit field extraction.
- **Cons:** Still redundant - the `.md` file has all of it. Still requires multi-parameter function signatures. Still no compile-time safety.
- **Why not chosen:** Doesn't solve the core problem. The LLM can read YAML frontmatter.

### Alternative 2: Compile-time placeholder validation via proc macro

- **Description:** A proc macro that parses `.pmt` files at compile time and verifies all `{...}` placeholders have corresponding `.replace()` calls.
- **Pros:** Catches stale placeholders at build time.
- **Cons:** Significant implementation effort. Proc macros add compile time.
- **Why not chosen:** Good future work but out of scope. The placeholder registry test in Phase 4 provides a lighter-weight safety net.

## Technical Considerations

### New helper: `read_full_markdown()`

`read_doc_content()` and `read_doc_content_or_empty()` strip the frontmatter and return only the body. But `{markdown_content}` must include the frontmatter - that's where title, order, status, AC, parent-id, and children live. We need:

```rust
/// Read the complete docs/loopr/<id>.md file including YAML frontmatter.
pub fn read_full_markdown(repo_path: &Path, id: &str) -> Result<String> {
    let path = repo_path.join("docs").join("loopr").join(format!("{}.md", id));
    fs::read_to_string(&path).map_err(|e| eyre!("{}: {}", path.display(), e))
}

pub fn read_full_markdown_or_empty(repo_path: &Path, id: &str) -> String {
    read_full_markdown(repo_path, id).unwrap_or_else(|e| {
        tracing::warn!("read_full_markdown failed for {}: {}", id, e);
        String::new()
    })
}
```

All `{markdown_content}` interpolation uses `read_full_markdown_or_empty()`. The existing `read_doc_content()` (body-only) stays for callers that need just the prose (e.g., context builder enrichment).

### Generation prompt: not template-based

`generation-work.pmt` is NOT interpolated via `.replace()`. The `build_work_prompt` function in `src/agents/generation.rs` concatenates sections programmatically and appends the `.pmt` content as trailing instructions. The phase content is injected via `push_str`, not template substitution. This means:
- The `.pmt` file itself has no `{markdown_content}` placeholder - it's just instructions
- The Rust code that builds the prompt must be updated to pass the full `.md` content instead of extracted fields
- References to "description" in the `.pmt` prose need rewriting, but no placeholder rename

### Dependencies

- No new crate dependencies.
- The `.md` files must exist at prompt build time. Use the `_or_empty` variants to avoid crashes.

### Performance

No impact. We're reading fewer files (one `.md` per entity instead of extracting multiple fields), doing fewer `.replace()` calls, and passing slightly larger strings to the LLM (frontmatter overhead is ~200 bytes).

### Testing Strategy

- Each phase must pass `otto ci`
- Existing prompt tests assert that placeholders are substituted (no `{...}` residue in output). These tests update to use the new placeholder names.
- Phase 4 adds a cross-cutting test that inventories all placeholders

### Two read patterns: body-only vs full-markdown

After this change, there are two read patterns:

| Function | Returns | Used by |
|---|---|---|
| `read_doc_content_or_empty()` | Body only (no frontmatter) | Context builder (assembles its own structured sections) |
| `read_full_markdown_or_empty()` | Complete `.md` file (frontmatter + body) | Prompt template interpolation (`{markdown_content}`) |

Both are valid. The context builder needs just the prose to embed in its structured context. The prompt templates need the full file because the LLM reads the frontmatter directly. Do not collapse these into one function.

### Implementer and reviewer: no changes

`implementer.pmt` and `reviewer.pmt` have no document-field placeholders. Work context (AC, files, dependencies) is injected by the context builder (`src/agents/context.rs`), not by `.replace()` on the template. These templates are used as-is as system prompts. No changes needed.

### Decompose templates: already correct

`decompose/spec.pmt`, `decompose/phase.pmt`, `decompose/work.pmt` already receive the parent's full markdown content as input (passed by the decomposer, not interpolated). No placeholders for document fields. No changes needed.

### Risk: LLM parsing YAML frontmatter

The LLM must correctly parse YAML frontmatter to extract title, order, AC, etc. This is a low risk - Claude handles YAML well and the frontmatter format is simple key-value pairs. If an issue surfaces, we can add a brief instruction in the `.pmt` telling the LLM how to read the frontmatter.

## Open Questions

None. Ship it.

## References

- `docs/design/2026-04-07-domain-model-cleanup.md` - the cleanup that created this need
- `docs/plan-spec-phase-work-markdown-locations.md` - supplemental map of all `.md` file locations
- Memory: `project_prompt_ssot.md` - prior note about magic strings in `.pmt` files
