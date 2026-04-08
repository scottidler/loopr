# Supplemental: Field Necessity Evaluation

**Author:** Scott A. Idler
**Date:** 2026-04-07
**Type:** Supplemental reference (field audit)
**Design Doc:** docs/design/2026-04-07-rip-out-staging-and-description.md

## Summary

Field-by-field evaluation of every field on Plan, Spec, Phase, and Work. The premise: once `docs/loopr/<id>.md` exists on disk (frontmatter with structured fields, body with prose and AC sections), which fields are still *necessary* anywhere in the system (JSONL, struct, SQLite), and which become redundant with what the .md already contains?

This is not "which fields belong on the struct." It is "which fields need to exist at all as separately-persisted data."

## Categories

Each field gets one of four verdicts:

- **Necessary** - No overlap with the .md, or the .md cannot serve this purpose (FSM state, timestamps)
- **Denormalized** - Present in the .md but kept separately for performance (title, order, tier). The .md is truth; the JSONL/struct copy is a cache. Could be eliminated if you accepted the disk-read cost.
- **Design question** - Present in the .md body AND used programmatically. The duplication is real: JSONL has structured data, .md has rendered prose. Keeping both means two sources of truth. Dropping the struct field means parsing markdown for programmatic use.
- **Redundant** - The .md IS this data. No reason to persist it separately.

## Plan

| Field | Type | Verdict | Rationale |
|-------|------|---------|-----------|
| `id` | `String` | **Necessary** | Primary key. In the filename too, but every lookup, IPC message, and DAG traversal needs it as a value. Cannot be eliminated. |
| `title` | `String` | **Denormalized** | In the .md (H1 or frontmatter). Kept on the struct for fast display in TUI lists, log lines, prompt context. If you removed it from JSONL, you would need to open and parse the .md for every list render. Performance cache, not independent data. |
| `description` | `String` | **Redundant** | The .md body IS the description. Already `skip_serializing`. No reason to exist. |
| `acceptance_criteria` | `AcceptanceCriteria` | **Design question** | Rendered in the .md body as a section. Also used programmatically: FSM gate (Work cannot enter Ready without AC), coverage evaluator, validation. If the .md is truth, this structured field is a parsed cache of what is in the .md. If the struct is truth, the .md rendering is a view. Currently the struct is truth and the .md renders it - but that means two sources that can drift. |
| `status` | `PlanStatus` | **Necessary** | FSM state. Changes independently of the .md content. Needs fast indexed queries (find all active plans). Cannot live in a .md file - mutable operational state does not belong in a content document. |
| `tier` | `Tier` | **Denormalized** | Small enum. Could be in .md frontmatter. Kept separately for fast dispatch routing. Same trade-off as title. |
| `created_at` / `updated_at` | `i64` | **Necessary** | Operational timestamps for ordering and staleness. Could theoretically use file mtime for `updated_at`, but file mtime is unreliable across git operations, restores, and copies. Must be explicitly tracked. |

## Spec

| Field | Type | Verdict | Rationale |
|-------|------|---------|-----------|
| `id` | `String` | **Necessary** | Primary key |
| `parent_id` | `String` | **Denormalized** | DAG link to Plan. In the .md frontmatter. Kept for fast relationship queries (find all specs for plan X) without opening every .md. |
| `title` | `String` | **Denormalized** | Same as Plan |
| `description` | `String` | **Redundant** | Same as Plan |
| `acceptance_criteria` | `AcceptanceCriteria` | **Design question** | Same as Plan |
| `status` | `SpecStatus` | **Necessary** | FSM state |
| `created_at` / `updated_at` | `i64` | **Necessary** | Timestamps |

## Phase

| Field | Type | Verdict | Rationale |
|-------|------|---------|-----------|
| `id` | `String` | **Necessary** | Primary key |
| `parent_id` | `String` | **Denormalized** | DAG link to Spec. Same as Spec's parent_id rationale. |
| `title` | `String` | **Denormalized** | Same as Plan |
| `description` | `String` | **Redundant** | Same as Plan |
| `order` | `u32` | **Denormalized** | Sequencing within a Spec. Could be in .md frontmatter. Kept for fast sorted queries without opening files. |
| `acceptance_criteria` | `AcceptanceCriteria` | **Design question** | Same as Plan |
| `status` | `PhaseStatus` | **Necessary** | FSM state |
| `validation_commands` | `Vec<String>` | **Redundant (legacy)** | Already `skip_serializing`. Pre-AC legacy field. Separate cleanup. |
| `created_at` / `updated_at` | `i64` | **Necessary** | Timestamps |

## Work

| Field | Type | Verdict | Rationale |
|-------|------|---------|-----------|
| `id` | `String` | **Necessary** | Primary key |
| `parent_id` | `String` | **Denormalized** | DAG link to Phase |
| `title` | `String` | **Denormalized** | Same as Plan |
| `description` | `String` | **Redundant** | Same as Plan |
| `assignee` | `Option<String>` | **Necessary** | Mutable operational state - which agent session owns this Work. Changes independently of .md content. Same argument as status: operational state does not belong in a content document. |
| `status` | `WorkStatus` | **Necessary** | FSM state (8-state machine) |
| `resource_tags` | `Vec<String>` | **Denormalized** | Scheduling affinity. Could live in .md frontmatter. Kept for fast lane-matching queries. |
| `dependencies` | `Vec<String>` | **Denormalized** | DAG edges (blocking logic). Could be in .md frontmatter. Kept for fast blocked/unblocked resolution without parsing files. Used in Ready transition checks. |
| `acceptance_criteria` | `AcceptanceCriteria` | **Design question** | Same as Plan, plus: AC is the FSM precondition for Work entering Ready. If AC only lived in the .md, the transition handler would need to parse markdown to validate the gate. |
| `checklist` | `Vec<ChecklistItem>` | **Design question** | Rendered in .md body. Also mutable structured data - each item has `completed: bool` that changes during execution. If the .md is truth, completion state would need to be parsed from `- [x]` / `- [ ]` checkboxes. If the struct is truth, the .md rendering can drift from actual completion state. |
| `created_at` / `updated_at` | `i64` | **Necessary** | Timestamps |

## Verdict Summary

| Verdict | Fields |
|---------|--------|
| **Necessary** | `id`, `status`, `created_at`/`updated_at`, `assignee` |
| **Denormalized** | `title`, `parent_id`, `tier`, `order`, `resource_tags`, `dependencies` |
| **Design question** | `acceptance_criteria` (all four), `checklist` (Work) |
| **Redundant** | `description` (all four), `validation_commands` (Phase, legacy) |

## The Real Questions

### 1. Denormalized fields - accept the duplication or eliminate it?

Six fields (`title`, `parent_id`, `tier`, `order`, `resource_tags`, `dependencies`) exist in the .md AND in JSONL/struct. If the .md is truth, the JSONL copies are caches. The trade-off:

- **Keep them**: fast queries, no file parsing for list views and DAG traversal. Accept that two copies can drift.
- **Drop them**: .md is the single source. Every query that needs these fields reads and parses .md files. Slower, but no drift.
- **Middle ground**: keep them in a lightweight index (SQLite) that is rebuilt from .md files on startup. Similar to how TaskStore rebuilds SQLite from JSONL today, but the source would be .md instead of JSONL.

### 2. AC and checklist - who owns the truth?

`acceptance_criteria` and `checklist` are the hardest calls. They are:
- Rendered in the .md body (human-readable)
- Used programmatically (FSM gates, coverage evaluation, completion tracking)
- Structured data that is lossy to round-trip through markdown

If the struct owns truth: the .md rendering is a view, and every .md regeneration overwrites the AC section. No parsing needed, but the .md is not independently editable.

If the .md owns truth: AC must be parsed from markdown for every programmatic use. Fragile, but the .md becomes the single source.

This is a design decision, not an obvious answer.

### 3. What is JSONL's role once .md files exist?

Today: JSONL is the gospel (TaskStore invariant). SQLite is a cache of JSONL. .md files are a rendered view.

With .md as source of truth for content: JSONL shrinks to operational state only (`id`, `status`, `assignee`, timestamps). The .md holds everything else. This inverts the current architecture - .md becomes gospel for content, JSONL becomes gospel for state.

Or: JSONL goes away entirely. The .md frontmatter holds ALL fields including status and timestamps. SQLite becomes a cache of .md files. This is the most radical option but also the simplest (one source of truth, period).

These are the real decisions this evaluation surfaces.
