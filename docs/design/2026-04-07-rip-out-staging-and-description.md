# Design Document: Rip Out Staging Files and Description Field

**Author:** Scott A. Idler
**Date:** 2026-04-07
**Status:** Implemented
**Supplemental:** docs/plan-spec-phase-work-markdown-locations.md

## Summary

Remove the decomposer's file round-trip, the `Doc` struct, the `description` field from domain types, and fix the test isolation leak. After this work, the decomposer operates entirely in memory until `persist_hierarchy` writes to JSONL and `docs/loopr/<id>.md`. All runtime reads of document content go through the filesystem, not the JSONL blob.

## What Gets Thrown Away

```
src/domain/doc.rs (entire file)
- Doc struct
- PlanDoc, SpecDoc, PhaseDoc, WorkDoc wrappers
- DocKind enum (if nothing outside decomposer needs it)
- DocKind::file_prefix()
- slug_from_title()
- doc_filename()
- write_doc_file()

src/decomposer.rs (specific items)
- DecomposedChild struct
- docs_to_hierarchy() function
- run_dir parameter from decompose_hierarchy(), decompose_into(), decompose_spec_branch(), ratify_hierarchy()
- All std::fs::read_to_string() calls that read staging files
- All staging dir creation (.staging-{parent.id})
- Vec<Doc> return value from decompose_hierarchy()

src/daemon/handlers/doc.rs (specific items)
- /tmp/loopr-run-<ts>/ creation at line 172
- write_doc_file() call for plan.md
- Doc creation in accept_plan_markdown()
- persist_doc() calls for Doc records
- Vec<Doc> handling from decompose_hierarchy result

src/domain/plan.rs, spec.rs, phase.rs, work.rs
- description field removed from all four structs
- description parameter removed from constructors
```

## What Stays

```
src/domain/markdown.rs (all of it)
- Markdown trait (renamed from DocMarkdown)
- write_markdown() (renamed from write_doc_markdown)
- read_markdown() (new - reads docs/loopr/<id>.md, errors if missing)
- format_frontmatter()
- FmValue, needs_quoting(), millis_to_iso()

src/domain/plan.rs, spec.rs, phase.rs, work.rs
- Structs (minus description)
- Markdown impls (body comes from docs/loopr/ file, not description field)
- Record impls
- FSM logic

src/decomposer.rs
- DecomposedHierarchy struct
- ChildEntry struct (content lives here in memory)
- All LLM call functions
- All prompt/validation/ratification functions (refactored to take &str not file paths)
- detect_cycles(), extract_acceptance_criteria(), child_kind()
- decomposition_tool_schema()

src/daemon/handlers/doc.rs
- handle_doc_inject(), handle_doc_accept() (refactored - no run_dir)
- accept_plan_markdown() (refactored - passes plan markdown as String, no file write)
- persist_hierarchy() (already correct - writes JSONL + docs/loopr/)
- extract_plan_title(), classify_brief()

src/daemon/handlers/{plan,spec,phase,work}.rs
- All handlers (create/get/list/transition/update)
- All write_markdown() calls

src/agents/context.rs
- ContextBuilder reads docs/loopr/<id>.md (already does this)
- Remove fallback to description field - error if file missing
```

## Renames

These renames happen as part of Phase 3/4 alongside the code they touch:

| Old Name | New Name | File |
|----------|----------|------|
| `DocMarkdown` trait | `Markdown` | `src/domain/markdown.rs` |
| `write_doc_markdown()` | `write_markdown()` | `src/domain/markdown.rs` |
| `doc_id()` | `id()` or keep | trait method on `Markdown` |
| `doc_frontmatter()` | `frontmatter()` or keep | trait method on `Markdown` |
| `doc_body()` | `body()` or keep | trait method on `Markdown` |

## Reference

The full code map of every struct, function, and file in the old and new paths is in the supplemental document:

**docs/plan-spec-phase-work-markdown-locations.md**

That document lists every item by file with OLD/NEW/BOTH classification. This design doc describes what to do with them; the supplemental describes where they are.

## Phases

### Phase 1: Fix test_stores() leak and clean up

Fix the root cause of 8,340 leaked files before anything else.

**Changes:**
- `src/daemon/handlers.rs:320` - `test_stores()` returns `(TestDir, Arc<Stores>)` with `repo_path` set to temp dir. Update all 77 callsites in plan.rs, spec.rs, phase.rs, work.rs handler tests.
- `rkvr rmrf docs/loopr/` in the source repo
- Add `docs/loopr/` to `.gitignore` in the loopr source repo as a safety net

**Validation:** `otto ci` passes. `docs/loopr/` does not reappear in source repo after test run.

### Phase 2: Fix AC duplication in doc_body()

**Changes:**
- `src/domain/markdown.rs` - add `pub fn strip_markdown_section(body: &str, section_title: &str) -> String` that removes a `## <title>` section and its content up to the next `## ` or end of string
- `src/domain/{plan,spec,phase,work}.rs` - in each `doc_body()`, call `strip_markdown_section(&self.description, "Acceptance Criteria")` before appending the checklist version

**Validation:** `otto ci` passes. Inspect a generated `docs/loopr/wk-*.md` - single AC section only.

### Phase 3: Rip out decomposer file round-trip

The big one. Make the decomposer work entirely in memory.

**Changes to `src/decomposer.rs`:**
- `decompose_into()` - receives parent content as `&str` parameter instead of reading from disk. Returns `Vec<(ChildEntry, typed record dependencies)>` instead of `Vec<Doc>`. No file writes. Build typed records (Spec/Phase/Work) directly from `ChildEntry.content`.
- `decompose_spec_branch()` - remove `run_dir` param. Pass content strings through.
- `decompose_hierarchy()` - remove `run_dir` param. Receives plan markdown as `&str`. Returns `DecomposedHierarchy` directly (no `Vec<Doc>`). Builds Plan from the input string. Passes ChildEntry content strings down through decompose_into/decompose_spec_branch.
- `ratify_hierarchy()` - remove `run_dir` param. Takes in-memory content map `HashMap<String, String>` (doc_id -> content) instead of reading files.
- `build_ratify_prompt()` - takes content strings, not file paths.
- Delete `docs_to_hierarchy()` entirely.
- Delete `DecomposedChild` struct.

**Changes to `src/daemon/handlers/doc.rs`:**
- `accept_plan_markdown()` - remove run_dir creation, remove write_doc_file call, remove Doc creation. Pass plan markdown string directly to `decompose_hierarchy()`.
- Remove `persist_doc()` calls for Doc records.
- `decompose_hierarchy` return type becomes `Result<(DecomposedHierarchy, Option<String>)>` - no `Vec<Doc>`.

**Changes to `src/domain/doc.rs`:**
- Delete `Doc`, `PlanDoc`, `SpecDoc`, `PhaseDoc`, `WorkDoc`
- Delete `slug_from_title()`, `doc_filename()`, `write_doc_file()`
- Keep `DocKind` and `DocKind::id_prefix()` only if referenced outside the deleted code. If not, delete the entire file.

**Validation:** `otto ci` passes. E2E run produces `docs/loopr/<id>.md` files in target repo. No `/tmp/loopr-run-*` directories created.

### Phase 4: Remove description field from domain structs

Remove `description` from Plan, Spec, Phase, Work. All callsites read from `docs/loopr/<id>.md` instead.

**Changes to `src/domain/markdown.rs`:**
- Add `pub fn read_doc_content(repo_path: &Path, id: &str) -> Result<String>` that reads `docs/loopr/<id>.md` and returns the body (everything after frontmatter). Errors if file is missing.

**Changes to `src/domain/{plan,spec,phase,work}.rs`:**
- Remove `description: String` field from each struct
- Remove `description` parameter from constructors (`Plan::new`, `Spec::new`, `Phase::new`, `Work::new`)
- Update `doc_body()` - body is no longer `self.description.clone()`. Instead, the full document content is the ChildEntry content that was persisted to `docs/loopr/<id>.md`. `doc_body()` returns just the AC checklist section (the structured data from the struct). The markdown body in the file IS the LLM content, written once by persist_hierarchy.
- Remove `description` from serde serialization (JSONL records shrink)

**Changes to 22 callsites that read `.description`:**
- `src/agents/context.rs:338,359,377,390,401` - read from `docs/loopr/<id>.md` via `read_doc_content()`. Remove description fallback.
- `src/agents/generation.rs:80,102` - read from filesystem
- `src/evaluator.rs:110` - read from filesystem
- `src/daemon/handlers/integrator.rs:308,328,348,428,438,464,473,499,505` - read from filesystem
- `src/domain/plan.rs:82` (tier classification) - caller must pass content string, not read from struct

**Changes to `src/daemon/handlers/doc.rs`:**
- `persist_hierarchy()` - write `ChildEntry.content` (the LLM markdown) as the body of `docs/loopr/<id>.md`. The domain struct no longer carries it.

**Validation:** `otto ci` passes. JSONL records have no `description` field. All agent context still populated correctly from filesystem.

### Phase 5: Update E2E path format

**Changes to `bin/e2e`:**
- Change run directory from `/tmp/loopr-e2e/<target>/<ts>/` to `/tmp/loopr/e2e/<target>/<ts>/`
- Update `latest` symlink accordingly

**Changes to e2e skill (`.claude/skills/e2e/`):**
- Update all path references from `/tmp/loopr-e2e/` to `/tmp/loopr/e2e/`

**No changes to loopr Rust code.** Loopr only knows about `repo_path` from config. The e2e script sets it.

**Validation:** `/e2e rust-version` runs. Files appear in `/tmp/loopr/e2e/rust-version/<ts>/docs/loopr/`.

## Phase Ordering

Each phase is independently shippable. `otto ci` after each.

```
Phase 1 (test leak)     - prerequisite for everything; stops the bleeding
Phase 2 (AC dedup)      - standalone fix, no dependencies
Phase 3 (staging rip)   - biggest change; decomposer refactor
Phase 4 (description)   - depends on Phase 3 (persist_hierarchy writes content to file)
Phase 5 (e2e path)      - standalone, can happen any time
```

## Open Questions

None. Ship it.
