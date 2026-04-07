# Supplemental: Rip Out Staging File Round-Trip

**Author:** Scott A. Idler
**Date:** 2026-04-07
**Type:** Supplemental reference (code map)
**Design Doc:** docs/design/2026-04-07-rip-out-staging-and-description.md
**Parent:** 2026-04-06-decomposer-direct-persistence.md

## Summary

The decomposer-direct-persistence design doc eliminated `double_write_old_records` but left the upstream file round-trip intact. The decomposer still writes LLM output to slug-named `.md` files on disk, wraps them in `Doc` records, then `docs_to_hierarchy` reads those files back to populate `description` on typed domain records. This is pointless I/O. The content is already in memory as a String - writing it to disk and reading it back is dead weight.

This document maps every struct, function, and file involved in the old staging path vs the new `docs/loopr/` path, so the old path can be surgically removed.

## Problem

The decomposer currently:

1. Calls the LLM and receives markdown text **in memory** (`ChildEntry.content`)
2. Writes that text to a slug-named file on disk (`write_doc_file` -> `plan-python-bookmarks-api.md`)
3. Creates a `Doc` record with a `markdown` field pointing to that filename
4. Later, `docs_to_hierarchy` reads those files back from disk (`std::fs::read_to_string`)
5. Passes the content into `Plan::new`, `Spec::new`, `Phase::new`, `Work::new` as `description`

Steps 2-4 are unnecessary. The content in step 5 is the exact same String from step 1.

This file round-trip also creates the `/tmp/loopr-run-<ts>/` staging directory (hardcoded in `doc.rs:172`), which:
- Leaks temporary files outside the target repo
- Contradicts the `document-architecture` design doc (which says staging goes in `.loopr/runs/`)
- Accumulates without cleanup

## Target State

Decomposer receives plan markdown as a String. Decomposes it into specs, phases, and works - all in memory. Builds `DecomposedHierarchy` directly. `persist_hierarchy` is the first and only place anything hits disk: JSONL via TaskStore and `docs/loopr/<id>.md` via `write_doc_markdown`.

No `Doc` intermediary. No staging directory. No slug-named files. No `/tmp/` paths in production code.

## Code Map

### OLD path (staging files - to be removed)

Slug-named markdown files written to a temporary `run_dir` during decomposition. Uses the `Doc` type and `write_doc_file`. Files like `plan-python-bookmarks-api.md`, `spec-api-routes.md`, `work-implement-get-health-endpoint.md`.

```
src/domain/doc.rs
- DocKind enum - Plan/Spec/Phase/Work variants
- DocKind::file_prefix() - returns "plan"/"spec"/"phase"/"work" slug prefixes
- Doc struct - stores .markdown filename reference to staging file
- Doc::new() - creates Doc with typed ID and filename
- PlanDoc, SpecDoc, PhaseDoc, WorkDoc - type-safe wrappers around Doc
- slug_from_title() - slugifies title for filenames
- doc_filename() - slug-based filename with collision detection
- write_doc_file() - writes markdown content to run_dir with slug filename

src/decomposer.rs
- ChildEntry struct - parsed LLM output; has .content String in memory
- DecomposedChild struct - wraps Doc + typed record
- decompose_into() - reads parent from disk, calls LLM, writes children to disk, returns Vec<Doc>
- decompose_spec_branch() - passes run_dir through to decompose_into
- decompose_hierarchy() - takes run_dir param, reads plan from disk, orchestrates decomposition
- docs_to_hierarchy() - reads staging files back from disk to build typed domain records
- ratify_hierarchy() - reads staging files from disk for ratification prompts
- build_ratify_prompt() - reads files to build ratification context

src/daemon/handlers/doc.rs
- accept_plan_markdown() - creates /tmp/loopr-run-<ts>/, writes plan.md via write_doc_file, creates Doc
```

### NEW path (docs/loopr/ - to keep)

Typed-ID markdown files written to `docs/loopr/` in the target repo. Uses `Markdown` trait and `write_doc_markdown`. Files like `pl-abc12.md`, `sp-def34.md`, `ph-56789.md`, `wk-aaa11.md`.

```
src/domain/markdown.rs
- FmValue enum - YAML frontmatter value types
- Markdown trait - doc_id(), doc_frontmatter(), doc_body()
- write_markdown() - writes docs/loopr/<id>.md with frontmatter + body
- format_frontmatter() - renders YAML frontmatter
- needs_quoting() - YAML quoting check
- millis_to_iso() - epoch ms to ISO 8601

src/domain/plan.rs
- Plan impl Markdown - frontmatter + body rendering

src/domain/spec.rs
- Spec impl Markdown - frontmatter + body rendering

src/domain/phase.rs
- Phase impl Markdown - frontmatter + body rendering

src/domain/work.rs
- Work impl Markdown - frontmatter + body rendering

src/daemon/handlers/doc.rs
- persist_hierarchy() - writes JSONL + docs/loopr/<id>.md for all records

src/daemon/handlers/plan.rs
- handle_plan_create() - calls write_doc_markdown on create
- handle_plan_transition() - calls write_doc_markdown on status change
- handle_plan_update() - calls write_doc_markdown on field update

src/daemon/handlers/spec.rs
- handle_spec_create() - calls write_doc_markdown
- handle_spec_transition() - calls write_doc_markdown
- handle_spec_update() - calls write_doc_markdown

src/daemon/handlers/phase.rs
- handle_phase_create() - calls write_doc_markdown
- handle_phase_transition() - calls write_doc_markdown
- handle_phase_update() - calls write_doc_markdown

src/daemon/handlers/work.rs
- handle_work_create() - calls write_doc_markdown
- handle_work_transition() - calls write_doc_markdown
- handle_work_update() - calls write_doc_markdown

src/agents/context.rs
- ContextBuilder reads docs/loopr/<id>.md at lines 579-607
- Falls back to description field when file is missing
```

### BOTH paths (keep, but refactor)

```
src/domain/doc.rs
- DocKind enum - still needed if anything references it outside decomposer
- DocKind::id_prefix() - used for typed ID generation (pl-, sp-, ph-, wk-)

src/domain/plan.rs
- Plan struct, HierarchyStatus, Tier - domain types, independent of file path

src/domain/spec.rs
- Spec struct - domain type

src/domain/phase.rs
- Phase struct - domain type

src/domain/work.rs
- Work struct, WorkStatus, ChecklistItem - domain types

src/decomposer.rs
- DecomposedHierarchy struct - keep, this is the in-memory output
- ChildEntry struct - keep, this already has content in memory
- call_llm_for_children() - keep, not file-dependent
- call_llm_for_validation() - keep, not file-dependent
- call_llm_for_ratification() - keep, not file-dependent
- call_llm_for_children_raw() - keep, raw LLM call
- build_decompose_prompt() - keep, takes &str not file path
- build_validate_prompt() - keep, takes &str
- detect_cycles() - keep, pure graph logic
- extract_acceptance_criteria() - keep, parses &str
- decomposition_tool_schema() - keep, JSON schema
- child_kind() - keep, pure mapping

src/daemon/handlers/doc.rs
- handle_doc_inject() - keep, but stop creating run_dir
- handle_doc_accept() - keep, but stop creating run_dir
- extract_plan_title() - keep, parses markdown string
- classify_brief() - keep, LLM tier classification
```

## Related Bugs Found During Audit

These should be fixed in the same pass:

1. **8,340 leaked files in source repo** - `test_stores()` in `handlers.rs:320` uses default `repo_path` (CWD = source repo). Every `otto test` run dumps `docs/loopr/` files into the loopr source repo. Fix: `test_stores()` must create a TestDir and set `repo_path`.

2. **AC written twice in docs/loopr markdown** - `doc_body()` in all four domain types clones `self.description` (which contains `## Acceptance Criteria` from the LLM), then appends another `## Acceptance Criteria` checklist. Fix: strip the section from description before appending.

3. **`description` field carries full markdown in JSONL** - redundant with `docs/loopr/<id>.md`. The field should shrink to a true one-liner once all callsites read from the filesystem. 22 callsites in context.rs, generation.rs, evaluator.rs, integrator.rs currently read it. context.rs already implements filesystem-first with fallback.

4. **Stale comment in doc.rs:17-18** - references deleted `coordinator.seed_manifest`.
