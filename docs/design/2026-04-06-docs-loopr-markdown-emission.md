# Design Document: docs/loopr/ Markdown Emission and Agent Context Delivery

**Author:** Scott A. Idler
**Date:** 2026-04-06
**Status:** Approved
**Review Passes Completed:** 5/5

## Summary

Every time Loopr creates or updates a Plan, Spec, Phase, or Work record, it writes a corresponding markdown file to `docs/loopr/` in the target repo. These files serve two purposes: (1) human-readable, Obsidian-compatible observability of the decomposition hierarchy, and (2) the primary delivery mechanism for agent context - agents read the full documents from disk instead of receiving truncated inline summaries.

## Problem Statement

### Background

Loopr persists all domain records (Plan, Spec, Phase, Work) as JSONL lines in `.taskstore/`. This is the authoritative data store - optimized for append-only durability and SQLite-cached queries. But JSONL is not human-readable, and the records are not accessible to agents as files on disk.

The e2e test infrastructure already creates timestamped directories at `/tmp/loopr-e2e/{target}/{YYYYMMDD-HHMMSS}/` that act as real git repos. Adding `docs/loopr/` inside these repos means every e2e run produces a browsable artifact.

### Problem

**Two problems, one root cause:**

1. **No human-readable output.** Operators must parse JSONL or use CLI commands to inspect decomposition results.

2. **Agent context truncation.** The decomposition pipeline spends LLM calls generating detailed Plan, Spec, Phase, and Work documents. But `ContextBuilder` (in `src/agents/context.rs`) inlines all four levels as one-liners (`**Plan:** title - desc`) into a single hierarchy section with a **2000 token budget**. Content exceeding this budget is hard-truncated. The detailed documents the decomposer produced are reduced to fragments before reaching the implementer.

The decomposition pipeline generates rich context. The context delivery pipeline throws it away.

### Goals

- Every Plan, Spec, Phase, and Work record has a corresponding `.md` file in `docs/loopr/`
- Files are written on create AND update (status changes, description edits)
- Files are named by typed ID: `pl-abc12.md`, `sp-def34.md`, `ph-56789.md`, `wk-aaa11.md`
- Each file has YAML frontmatter with structured metadata and the description as markdown body
- `ContextBuilder` interpolates the full Work doc into the agent's prompt (untruncated) and provides parent docs as markdown links
- Agents use their existing `read_file` tool to read parent documents if needed
- Flat directory - no subdirectories
- Works for all Loopr deployments, not just e2e
- Obsidian-compatible (valid YAML frontmatter, standard markdown)

### Non-Goals

- Obsidian-specific features (wikilinks, dataview queries, plugins) - just standard markdown
- Git committing the docs - Loopr writes files; the operator (or e2e script) decides whether to commit them
- Replacing `.taskstore/` - JSONL remains the source of truth for persistence
- Real-time Obsidian sync or file watching

## Binding Decisions

These decisions were reached through architectural review and resolve all open questions from earlier drafts.

### 1. `docs/loopr/` and `.gitignore`

**DO NOT add `docs/loopr/` to `.gitignore` automatically.** Loopr is an orchestration tool; the developer owns the repository. If they want to track the generated documentation in git (as a living architectural artifact), they must be allowed to do so.

### 2. `.loopr/runs/` writer deprecated

The existing title-slugged, run-scoped markdown writer in `src/domain/doc.rs` (`write_doc_file()`, `create_run_dir()`) is redundant. Two systems emitting markdown representations of the same data is tech debt. Strip it out entirely as part of this work.

### 3. Context delivery: Work interpolated, parents as links

Only the Work doc is interpolated into the agent's prompt. Parent docs (Phase, Spec, Plan) are provided as standard markdown links. The agent follows them with `read_file` if it needs broader context. Interpolating parents reintroduces the token-budget truncation problem this work is solving.

### 4. No token cap on the Work doc

The entire point of this refactor is to stop truncating the agent's instructions. If the decomposer writes a 4000-token Work document, it did so for a reason. The hierarchy token budget is eliminated. Other runtime state sections (learnings, tools, previous summary) retain their existing token budgets.

### 5. ContextBuilder name stays

The name `ContextBuilder` stays for now. Its role narrows - it no longer assembles "what to build" (that comes from the Work doc on disk). It retains responsibility for runtime state: rejections, active locks, sibling agents, tools, learnings, staleness warnings, iteration history, budget warnings. A rename may happen later once the role is fully settled.

### 6. Decomposer tool schema update is part of this work

The decomposer's JSON tool schema (`src/decomposer.rs`) and `.pmt` prompt templates must be updated to include `acceptance_criteria` as an `array` of `string`s for all four levels. Deserialization maps the array into the `AcceptanceCriteria` struct.

## Proposed Solution

### Overview

Add a `write_doc_markdown()` function that accepts any domain record implementing a new `DocMarkdown` trait and writes it to `{repo_path}/docs/loopr/{id}.md`. Call this function from each handler immediately after the successful TaskStore write.

### The DocMarkdown Trait

```rust
/// Values that can appear in YAML frontmatter.
pub enum FmValue {
    Text(String),
    List(Vec<String>),
}

pub trait DocMarkdown {
    fn doc_id(&self) -> &str;
    fn doc_frontmatter(&self) -> Vec<(String, FmValue)>;
    fn doc_body(&self) -> String;  // owned - allows appending checklist to description
}
```

Implemented by Plan, Spec, Phase, and Work. The trait keeps the markdown writer generic - no match arms on type.

Example impl for Plan:

```rust
impl DocMarkdown for Plan {
    fn doc_id(&self) -> &str { &self.id }
    fn doc_body(&self) -> String { self.description.clone() }
    fn doc_frontmatter(&self) -> Vec<(String, FmValue)> {
        let mut m = Vec::new();
        m.push(("id".into(), FmValue::Text(self.id.clone())));
        m.push(("title".into(), FmValue::Text(self.title.clone())));
        m.push(("status".into(), FmValue::Text(format!("{:?}", self.status()))));
        m.push(("tier".into(), FmValue::Text(format!("{:?}", self.tier))));
        m.push(("acceptance-criteria".into(), FmValue::List(self.acceptance_criteria.0.clone())));
        m.push(("created-at".into(), FmValue::Text(millis_to_iso(self.created_at))));
        m.push(("updated-at".into(), FmValue::Text(millis_to_iso(self.updated_at))));
        m
    }
}
```

Work uses `FmValue::List` for its Vec fields and appends checklist to the body:

```rust
impl DocMarkdown for Work {
    fn doc_id(&self) -> &str { &self.id }
    fn doc_body(&self) -> String {
        let mut body = self.description.clone();
        if !self.checklist.is_empty() {
            body.push_str("\n\n## Checklist\n\n");
            for item in &self.checklist {
                let mark = if item.completed { "x" } else { " " };
                body.push_str(&format!("- [{}] {}\n", mark, item.description));
            }
        }
        body
    }
    fn doc_frontmatter(&self) -> Vec<(String, FmValue)> {
        let mut m = Vec::new();
        // ... scalar fields as FmValue::Text ...
        m.push(("resource-tags".into(), FmValue::List(self.resource_tags.clone())));
        m.push(("dependencies".into(), FmValue::List(self.dependencies.clone())));
        m.push(("acceptance-criteria".into(), FmValue::List(self.acceptance_criteria.0.clone())));
        m
    }
}
```

### File Format

```markdown
---
id: pl-pakjt
title: Add --version flag to CLI
status: Active
tier: Full
acceptance-criteria:
  - "CLI prints version string matching git describe output"
  - "Version includes git hash suffix"
created-at: 2026-04-06T14:30:22Z
updated-at: 2026-04-06T14:31:05Z
---

Full description text from the record's `description` field.

Multiple paragraphs, markdown formatting, code blocks - whatever
the LLM generated.

## Acceptance Criteria

- [ ] CLI prints version string matching git describe output
- [ ] Version includes git hash suffix
```

Notes:
- Frontmatter keys use kebab-case per project convention
- Timestamps converted from epoch millis to ISO 8601
- `parent-id` included for Spec, Phase, and Work (not Plan)
- `status` is the human-readable enum variant name
- Frontmatter values containing special YAML characters are quoted
- AC appears in both frontmatter (structured data) and body (human-readable checkboxes)

### Type-Specific Frontmatter

**Plan:**
```yaml
id, title, status, tier, acceptance-criteria, created-at, updated-at
```

**Spec:**
```yaml
id, parent-id, title, status, acceptance-criteria, created-at, updated-at
```

**Phase:**
```yaml
id, parent-id, title, status, order, acceptance-criteria, created-at, updated-at
```

**Work:**
```yaml
id, parent-id, title, status, assignee, resource-tags, dependencies, acceptance-criteria, created-at, updated-at
```

### Acceptance Criteria Type

Replace the inconsistent `String` / `Vec<String>` / missing fields with a single wrapper type used at all four hierarchy levels.

```rust
// src/domain/criteria.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AcceptanceCriteria(pub Vec<String>);
```

Update `Plan`, `Spec`, `Phase`, and `Work` to use this type:
`pub acceptance_criteria: AcceptanceCriteria`

The `DocMarkdown` trait renders AC as `- [ ]` checkboxes in the markdown body for all four doc types.

### AC Semantics Per Level

AC means different things at different levels of the hierarchy. The evaluation mechanism varies accordingly.

| Level | What AC contains | Evaluated by | Mechanism |
|-------|-----------------|-------------|-----------|
| **Work** | Concrete assert statements about code changes | Reviewer agent | LLM reads diff + assertions, judges pass/fail |
| **Phase** | Shell commands that must exit 0 (absorbs `validation_commands`) | Automated runner | Execute commands after all Work items complete |
| **Spec** | Integration test commands that must pass | Automated runner | Execute after all Phases complete, before Spec marked Done |
| **Plan** | User stories restated as assertions | Human | Structured handoff - Loopr presents checklist, human evaluates |

**Phase AC absorbs `validation_commands`.** The `validation_commands: Vec<String>` field on Phase is removed. Phase-level AC items are shell commands (e.g. `cargo test --lib`, `cargo check`). The description of Phase AC in decomposer prompts should make this explicit: "acceptance criteria for phases are shell commands that validate the combined output of the phase's work items."

**Plan AC is human-evaluated.** When all Specs complete, Loopr presents the Plan's AC as a checklist for the operator to verify. The automated evaluation mechanism for Plan AC is deferred - a lightweight model evaluating user-story-level assertions against observed output is possible but not part of this work.

### Write Function

```rust
pub fn write_doc_markdown(repo_path: &Path, record: &impl DocMarkdown) -> Result<()> {
    let dir = repo_path.join("docs").join("loopr");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", record.doc_id()));
    let content = format!("---\n{}---\n\n{}\n",
        format_frontmatter(&record.doc_frontmatter()),
        record.doc_body()
    );
    fs::write(&path, content)?;
    Ok(())
}
```

**Key ordering:** `Vec<(String, FmValue)>` preserves insertion order so `id` and `title` appear at the top for human readability.

The `format_frontmatter` helper emits one entry per line. Scalar values containing `:`, `#`, `[`, `]`, `{`, `}`, or newlines are double-quoted. List values use YAML block syntax.

```rust
fn format_frontmatter(fields: &[(String, FmValue)]) -> String {
    let mut out = String::new();
    for (k, v) in fields {
        match v {
            FmValue::Text(s) => {
                if needs_quoting(s) {
                    out.push_str(&format!("{}: \"{}\"\n", k, s.replace('"', "\\\"")));
                } else {
                    out.push_str(&format!("{}: {}\n", k, s));
                }
            }
            FmValue::List(items) if items.is_empty() => {
                out.push_str(&format!("{}: []\n", k));
            }
            FmValue::List(items) => {
                out.push_str(&format!("{}:\n", k));
                for item in items {
                    out.push_str(&format!("  - \"{}\"\n", item.replace('"', "\\\"")));
                }
            }
        }
    }
    out
}
```

The `millis_to_iso` helper converts epoch milliseconds to ISO 8601 UTC using `chrono`:

```rust
fn millis_to_iso(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| ms.to_string())
}
```

This is a synchronous `std::fs::write` - the write is small (a few KB) and non-blocking in practice. No need for async I/O here.

### Integration Points

Each handler calls `write_doc_markdown` **after** both the TaskStore write and the in-memory insert succeed, but **before** the event broadcast. The markdown write is outside any lock scope - it is advisory and must not block or fail the handler.

The pattern at each call site:

```rust
// 1. TaskStore write (under store lock)
store.lock()?.create(record.clone())?;
// 2. In-memory insert (under map lock)
stores.write_plans()?.insert(id.clone(), record.clone());
// 3. Markdown emission (no lock, log-and-continue on failure)
if let Err(e) = write_doc_markdown(&stores.config.project.repo_path, &record) {
    tracing::warn!("docs/loopr write failed for {}: {e}", record.id);
}
// 4. Event broadcast
let _ = event_tx.send(DaemonEvent::record_created("plan", &id));
```

Note: `status` is a private field on all four types, accessed via `.status()` method. The `DocMarkdown` impl calls `.status()` to get the display value.

**plan.rs** - `handle_plan_create()`, `handle_plan_update()`, `handle_plan_transition()`

**spec.rs** - `handle_spec_create()`, `handle_spec_update()`, `handle_spec_transition()`

**phase.rs** - `handle_phase_create()`, `handle_phase_update()`, `handle_phase_transition()`

**work.rs** - `handle_work_create()`, `handle_work_update()`, `handle_work_transition()`

That's 12 call sites total (3 per type x 4 types). A helper macro or function in the handler module could reduce the boilerplate, but 12 one-liners is manageable.

### ContextBuilder Changes

`ContextBuilder` (in `src/agents/context.rs`) changes how it delivers context to agents.

Replace the current hierarchy section in `ContextBuilder::build()` (lines 574-623):

**Before (current):**
```markdown
## Hierarchy

**Plan:** Add CLI version flag - Full description crammed onto one line and truncated...
**Spec:** CLI argument parsing - Another description truncated...
**Phase:** Implement --version - Truncated...
**Work:** Add version string - Truncated at 2000 tok...
```

**After:**
```markdown
## Your Assignment

{full contents of docs/loopr/wk-1adga.md interpolated here by ContextBuilder}

## Parent Context (read if needed)

- [Plan: Add CLI version flag](docs/loopr/pl-pakjt.md)
- [Spec: CLI argument parsing](docs/loopr/sp-5ol89.md)
- [Phase: Implement --version](docs/loopr/ph-7cnc7.md)
```

Key changes:
- The Work doc is **interpolated into the prompt** - `ContextBuilder` reads `docs/loopr/wk-xxx.md` from disk and injects the full content
- Parent docs are standard markdown links the agent can `read_file` if needed
- No `truncate_prose()` on the hierarchy section
- The hierarchy token budget is eliminated - the Work doc is included in full, always
- The `.pmt` template gets a `{work_doc}` slot that `ContextBuilder` fills before the LLM call
- `ContextBuilder` retains all other sections: guidance, learnings, tools, state summary, staleness, previous iteration, sibling works, iteration counter, footer

### `.loopr/runs/` Removal

Remove the following from `src/domain/doc.rs`:
- `create_run_dir()` function
- `write_doc_file()` function
- Any supporting helpers (slug generation, collision handling)

Remove call sites in `src/daemon/handlers/doc.rs`:
- `handle_doc_accept` and `handle_doc_inject` references to run directory creation
- `accept_plan_markdown` references to `write_doc_file()`
- `double_write_old_records()` references to run directory reads

The `Doc` domain type itself may still be useful for tracking accepted plans; only the filesystem emission to `.loopr/runs/` is removed.

### Where the Code Lives

- `src/domain/markdown.rs` - the `DocMarkdown` trait, `write_doc_markdown()`, `format_frontmatter()` helper
- `src/domain/criteria.rs` - the `AcceptanceCriteria` wrapper type
- `src/domain/{plan,spec,phase,work}.rs` - trait impls, AC field changes
- `src/daemon/handlers/{plan,spec,phase,work}.rs` - handler integration (write file after store persist)
- `src/agents/context.rs` - context builder rewrite (interpolated Work doc + parent links)
- `src/decomposer.rs` - tool schema update for AC at all levels

### E2E Impact

None. The e2e script already creates `/tmp/loopr-e2e/{target}/{YYYYMMDD-HHMMSS}/` as the target repo. The daemon starts with `repo_path` pointing there. `docs/loopr/` appears automatically inside it. No e2e script changes needed.

After an e2e run:
```
/tmp/loopr-e2e/rust-version/20260406-143022/
  .git/
  .taskstore/
  .worktrees/
  docs/loopr/          <- NEW
    pl-pakjt.md
    sp-5ol89.md
    ph-7cnc7.md
    wk-1adga.md
    wk-2beb3.md
  loopr.yml
  src/
  Cargo.toml
```

Point an Obsidian vault at `/tmp/loopr-e2e/rust-version/20260406-143022/` and browse `docs/loopr/` as rendered markdown.

## Alternatives Considered

### Alternative 1: Post-run export command (`loopr export --obsidian`)

- **Description:** A CLI command that reads `.taskstore/` JSONL and generates markdown files after the fact.
- **Pros:** No handler changes. Can re-export any time. Can customize format per invocation.
- **Cons:** Must remember to run it. Doesn't capture intermediate states (only final state). Extra CLI surface area.
- **Why not chosen:** We want docs updated on every state change, not just at the end. The handler approach captures the full lifecycle.

### Alternative 2: Extend existing `.loopr/runs/` doc writer

- **Description:** The existing `write_doc_file()` in `src/domain/doc.rs` writes docs to `.loopr/runs/YYYYMMDD-HHMMSS/`. Extend it to also write to `docs/loopr/`.
- **Pros:** Reuses existing code path.
- **Cons:** The existing writer uses title-slugs for filenames, is scoped per-run, and has a different purpose (run artifacts vs. living documents). Conflating them creates confusion.
- **Why not chosen:** Different purposes warrant different code paths. The run-scoped writer is being deprecated as part of this work.

### Alternative 3: TaskStore hook / event-driven writer

- **Description:** Subscribe to `DaemonEvent::record_created` / `record_updated` events and write markdown from an event handler.
- **Pros:** Decoupled from handlers. Single integration point.
- **Cons:** Events are fire-and-forget; no guarantee of ordering or delivery. The event payload may not carry the full record. Adds indirection.
- **Why not chosen:** Direct handler integration is simpler, more reliable, and easier to reason about.

## Technical Considerations

### Dependencies

- No new crate dependencies. Uses `std::fs`, existing `chrono` for epoch-to-ISO conversion.

### Performance

- Each write is a single `fs::write()` of a few KB. Negligible overhead compared to the LLM calls that generate the records.
- `create_dir_all` is idempotent and cheap after the first call.

### Failure Mode

- If `write_doc_markdown` fails (disk full, permissions), it should log a warning and continue - markdown emission is advisory, not load-bearing. A failed doc write must NOT prevent the TaskStore write or daemon operation.
- Use `.context("writing doc markdown")` and log the error, don't propagate it to the handler's return value.

### Testing Strategy

- Unit test `DocMarkdown` implementations: given a Plan/Spec/Phase/Work struct, assert the frontmatter map and body are correct.
- Unit test `write_doc_markdown`: write to a `tempdir`, read back, verify format.
- Unit test `format_frontmatter`: verify YAML escaping, key ordering.
- Integration: existing e2e runs will produce `docs/loopr/` - verify files exist and are non-empty.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Markdown write fails silently | Low | Low | Log warning with path and error |
| Frontmatter YAML escaping bugs | Medium | Low | Unit test with special characters in titles |
| Stale docs if record deleted | Low | Low | Records are rarely deleted; add cleanup later if needed |
| Large descriptions slow write | Low | Low | fs::write is buffered; descriptions are ~1-10 KB |
| Doc file missing when agent reads | Low | Medium | Handler writes on every create/update; path is deterministic from ID |

## Implementation Plan

### Phase 1: Trait, writer, and AC type

- Define `AcceptanceCriteria` wrapper in `src/domain/criteria.rs`
- Define `DocMarkdown` trait and `FmValue` enum in `src/domain/markdown.rs`
- Implement `format_frontmatter()`, `write_doc_markdown()`, `millis_to_iso()`
- Unit tests for the writer, frontmatter formatting, and AC serialization

### Phase 2: Domain struct changes and trait implementations

- Add `acceptance_criteria: AcceptanceCriteria` to Spec and Phase
- Change Plan's `acceptance_criteria` from `String` to `AcceptanceCriteria`
- Remove `validation_commands: Vec<String>` from Phase (absorbed into AC)
- Implement `DocMarkdown` for Plan, Spec, Phase, Work
- Update `#[serde(default)]` for backwards compatibility with existing JSONL
- Unit tests for each implementation (frontmatter contents, body rendering, checklist)

### Phase 3: Decomposer and handler integration

- Update decomposer tool schema to include `acceptance_criteria` array for all levels
- Update `.pmt` prompt templates for spec/phase decomposition to generate AC
- Add `write_doc_markdown` calls to all 12 handler sites (create/update/transition x 4 types)
- Log-and-continue on failure
- Remove `.loopr/runs/` writer (`create_run_dir`, `write_doc_file`, call sites)

### Phase 4: ContextBuilder rewrite

- Replace inline hierarchy section with interpolated Work doc + parent links
- Read `docs/loopr/wk-xxx.md` from disk, inject into prompt
- Eliminate hierarchy token budget
- Add `{work_doc}` slot to `.pmt` template
- Keep acceptance criteria and resource tags inline as fallback metadata
- Retain all other sections (guidance, learnings, tools, state summary, etc.)

### Phase 5: Verification

- Run e2e, verify `docs/loopr/` appears with correct files
- Verify implementer receives full Work doc in prompt
- Point Obsidian vault at e2e output, confirm rendering
- Compare implementer output quality before/after (truncated inline vs full doc)

## References

- `src/domain/doc.rs` - existing run-scoped doc writer (to be deprecated)
- `src/daemon/handlers/{plan,spec,phase,work}.rs` - handler integration points
- `src/domain/{plan,spec,phase,work}.rs` - domain struct definitions
- `src/agents/context.rs:574-623` - current hierarchy inlining (to be replaced)
- `src/agents/context.rs:143-205` - token budgets per role
- `prompts/implementer.pmt` - implementer system prompt (needs update for file reads)
- `src/decomposer.rs:180-222` - decomposer tool schema (needs AC for all levels)
- `bin/e2e` - e2e script that creates timestamped target repos
- `docs/design/2026-02-25-orchestration-spine.md` - TaskStore architecture
