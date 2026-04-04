# Design Document: Document Architecture Refactor

**Author:** Scott Idler + Claude
**Date:** 2026-04-04
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Replace Loopr's four separate domain structs (Plan, Spec, Phase, Work) with a unified `Doc` storage type and thin domain wrappers. Move artifact content out of JSON struct fields and into .md files on disk. Replace `order` with `dependencies`. Extract decomposition from the Coordinator into a standalone Decomposer. Clarify the three plan entry paths (chat funnel, E2E test, manifest).

## Problem Statement

### Background

Loopr's Plan -> Spec -> Phase -> Work hierarchy is implemented as four separate Rust structs, each storing content in `title: String` and `description: String` fields serialized into TaskStore JSONL records. The Coordinator agent is responsible for interviewing the user, decomposing plans, AND executing the resulting hierarchy - three distinct jobs in one agent. The decomposition pipeline is the least reliable part of the system, leading to a YAML manifest bypass that pre-specifies the entire hierarchy.

### Problem

1. **Four nearly identical structs.** Plan, Spec, Phase, and Work have the same shape (id, parent_id, status, title, description, timestamps) with minor field variations. This means four sets of handlers, four sets of tests, four Record impls, all doing the same thing.

2. **Content trapped in JSON fields.** The substance of each artifact (requirements, architecture, acceptance criteria) lives in a `description: String` field inside a JSONL record. This means:
   - Not human-readable without tooling
   - Not editable without IPC
   - Not diffable in git
   - No template enforcement - the LLM writes whatever it wants
   - Agents must receive content via JSON actions, not by reading files like they do with code

3. **Coordinator does three jobs.** The Coordinator currently interviews, decomposes, AND executes. The chat funnel design doc already established that interviewing belongs to the Chat agent. Decomposition and execution are separate concerns - the Decomposer needs different prompts, models, and retry logic than the Coordinator's execution loop.

4. **`order` is too rigid.** Phases have `order: u32` for linear sequencing. Works have `dependencies: Vec<String>`. These should be the same mechanism - a dependency DAG works for both linear and parallel execution.

5. **Decomposition doesn't work reliably.** The YAML manifest exists because LLM decomposition fails often enough that E2E tests can't depend on it. The Decomposer must be a first-class, testable, retriable system - not a side effect of the Coordinator's iteration loop.

### Goals

- G1: Unified `Doc` storage struct with `DocKind` enum and thin `Plan`/`Spec`/`Phase`/`Work` domain wrappers
- G2: Artifact content stored as .md files on disk, referenced by `markdown` field on `Doc`
- G3: `dependencies: Vec<String>` replaces `order: u32` at all levels
- G4: Standalone Decomposer (system call, not agent) that takes a document at any level and produces children
- G5: Coordinator receives a fully decomposed hierarchy and only executes - no interviewing, no decomposing
- G6: Three clear plan entry paths: chat funnel (primary), E2E test injection (secondary), manifest (tertiary)
- G7: Level-by-level decomposition with per-level validation and a final ratification pass

### Non-Goals

- Changing the Chat agent or TUI chat funnel UX
- Modifying Bundle, Tick, Learning, or Lock domain types
- Redesigning the Coordinator's execution FSM (ActivatePhase, Executing, PhaseGate, GoalComplete)
- Building the bidirectional chat-orchestration bridge (event streaming back to chat)
- Implementing `/pause`, `/stop`, `/status` during execution
- Removing the `Interviewing` FSM state (it remains for headless/Auto mode; separate decision)

## Proposed Solution

### Overview

Three changes, layered:

1. **Doc struct + files on disk** - Unified storage, .md files as artifacts
2. **Decomposer** - Standalone system call for plan decomposition
3. **Coordinator scope reduction** - Coordinator executes only; does not decompose

### Architecture

#### The Plan Entry Pipeline

Reference: `docs/2026-04-04-chat-tunnel-vs-e2e-insertion-for-entering-a-plan.md`

```
  PRIMARY: Chat Funnel                    SECONDARY: E2E Test
  ========================                ====================
  User chats with Chat agent              Test provides a plan.md
  User types /plan                        directly
  Interview refines the plan              |
  User types /draft                       |
  User reviews, edits                     |
  User types /accept                      |
         |                                |
         v                                v
  ┌─────────────────────────────────────────┐
  │  Plan .md written to disk               │
  │  Doc record created (Draft)             │
  └──────────────────┬──────────────────────┘
                     │
                     v  User activates plan
  ┌─────────────────────────────────────────┐
  │  Decomposer                             │
  │  Plan -> Specs (validate each)          │
  │  Specs -> Phases (validate each)        │
  │  Phases -> Works (validate each)        │
  │  Final: Ratifier checks all docs        │
  └──────────────────┬──────────────────────┘
                     │
                     v
  ┌─────────────────────────────────────────┐
  │  Coordinator                            │
  │  Receives fully decomposed hierarchy    │
  │  Sequences phases, assigns agents       │
  │  Monitors, retries, gates              │
  └─────────────────────────────────────────┘
```

The YAML manifest is a third path that injects a pre-decomposed hierarchy, skipping the Decomposer. It is valid for human-authored decompositions and testing the execution pipeline in isolation.

#### Component Responsibilities

| Component | Does | Does NOT |
|-----------|------|----------|
| Chat agent | Interviews user, produces plan .md | Decompose. Execute. |
| Decomposer | Takes a doc, produces child docs | Interview. Execute. |
| Coordinator | Executes a decomposed hierarchy | Interview. Decompose. |

No component does another component's job. Ever.

### Data Model

#### DocKind Enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocKind {
    Plan,
    Spec,
    Phase,
    Work,
}
```

#### Doc Struct (storage layer)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Doc {
    pub id: String,
    pub kind: DocKind,
    pub parent_id: Option<String>,
    pub markdown: String,
    pub dependencies: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
```

- `id`: TaskStore ID with kind-based prefix (`pl-`, `sp-`, `ph-`, `wk-`)
- `kind`: Which level of the hierarchy
- `parent_id`: `None` for Plan, `Some` for Spec/Phase/Work
- `markdown`: Relative file path within the run directory (e.g., `spec-core-implementation.md`)
- `dependencies`: Vec of sibling Doc IDs that must be complete before this doc can proceed
- `acceptance_criteria`: Structured, queryable conditions that define "done." Parsed from the .md file at creation time by the Decomposer. Agents and validators check these programmatically without parsing markdown.

Doc has **no status field**. It is pure data. `Doc` implements `Record` for TaskStore. One collection: `"docs"`.

#### Domain Wrappers Own Their Status

Each wrapper type has its own status enum appropriate to its lifecycle:

```rust
pub struct Plan {
    pub doc: Doc,
    pub status: HierarchyStatus,  // Draft, Active, Complete, Abandoned
}

pub struct Spec {
    pub doc: Doc,
    pub status: HierarchyStatus,
}

pub struct Phase {
    pub doc: Doc,
    pub status: HierarchyStatus,
}

pub struct Work {
    pub doc: Doc,
    pub status: WorkStatus,  // Ready, InProgress, Blocked, InReview, Integrated, Done, Abandoned
}
```

Plan/Spec/Phase share `HierarchyStatus` (Draft, Active, Complete, Abandoned). Work has `WorkStatus` with its richer execution-phase FSM. No type ever sees states it can't enter. The compiler enforces this - not runtime checks.

**Persistence:** The wrapper types implement `Record`, not `Doc`. Each wrapper serializes its `doc` fields plus its own `status`. When loading from TaskStore, the `kind` field on the inner Doc determines which wrapper to construct. Four collections (`"plans"`, `"specs"`, `"phases"`, `"works"`) or one collection with kind-based deserialization - implementation detail.

#### Data Ownership: File vs Struct

The .md file and the Doc struct serve different purposes. They are NOT dual sources of truth:

| Concern | Owner | Why |
|---------|-------|-----|
| Content (requirements, architecture, steps) | .md file | Human/LLM-authored. Editable. Diffable. |
| Lifecycle (status, timestamps) | Doc struct | System-managed. Agents don't edit these. |
| Structure (parent_id, dependencies) | Doc struct | System-managed. IDs are opaque to humans. |
| Acceptance criteria | Both | Authored in .md, parsed into `Vec<String>` on Doc at creation time for programmatic checking. |

The .md file does NOT contain frontmatter, IDs, or status. The Doc struct does NOT contain prose content. They don't drift because they own different things. Acceptance criteria is the one field that crosses the boundary - it's authored in the file but extracted into the struct because validators and agents need to check it without parsing markdown.

#### Domain Wrappers (type safety layer)

```rust
pub struct Plan(pub Doc);
pub struct Spec(pub Doc);
pub struct Phase(pub Doc);
pub struct Work(pub Doc);

impl Plan {
    pub fn new(markdown: String) -> Self {
        Self(Doc::new(DocKind::Plan, None, markdown))
    }
}

impl Spec {
    pub fn new(parent_id: String, markdown: String) -> Self {
        Self(Doc::new(DocKind::Spec, Some(parent_id), markdown))
    }
}

// Phase and Work follow the same pattern
```

Newtype wrappers. Functions that operate on a specific level take the wrapper type. Construction validates that the `DocKind` matches. All common operations (status transitions, dependency checks, timestamps) go through `self.0` (the inner `Doc`). If a level ever needs level-specific fields, they go on the wrapper struct alongside the `Doc`.

**Persistence:** Only `Doc` implements `Record`. The wrappers are for in-memory type safety. When persisting, unwrap to `Doc`. When loading, wrap based on `kind` field.

#### What Fields Were Removed

| Old Field | Where It Went |
|-----------|--------------|
| `title: String` | In the .md file (first `#` heading) |
| `description: String` | In the .md file (body content) |
| `acceptance_criteria: String` (Plan) | `acceptance_criteria: Vec<String>` on Doc (parsed from .md at creation) |
| `acceptance_criteria: Vec<String>` (Work) | `acceptance_criteria: Vec<String>` on Doc (same field, all levels) |
| `order: u32` (Phase) | Replaced by `dependencies: Vec<String>` |
| `tier: Tier` (Plan) | In the .md file (tier gate determines this at decomposition time) |
| `validation_commands: Vec<String>` (Phase) | In the .md file (## Validation section) |
| `assignee: Option<String>` (Work) | Stays on Work wrapper if needed during execution, or tracked by Coordinator state |
| `resource_tags: Vec<String>` (Work) | In the .md file |
| `checklist: Vec<ChecklistItem>` (Work) | In the .md file |

### Files on Disk

#### Run Directory

Each orchestration run gets a flat directory:

```
.loopr/runs/20260404-143022/
  plan-parallel-validation.md
  spec-core-implementation.md
  spec-api-integration.md
  phase-data-model.md
  phase-validation.md
  phase-endpoints.md
  work-create-schema.md
  work-add-indexes.md
  work-input-validation.md
  work-auth-endpoint.md
  work-query-endpoint.md
```

- Directory name: `YYYYMMDD-HHMMSS` format
- Filenames: `<kind>-<slugified-title>.md`
- All files flat in one directory. No nesting.
- The `markdown` field on Doc stores the filename relative to the run directory
- Slug collisions: append `-2`. Log a warning. Should be rare.

#### File Content

Pure markdown. No frontmatter. No IDs. The file is the artifact.

Content follows the templates in `docs/templates/` (plan.md, spec.md, phase.md, work.md). The Decomposer uses these templates as part of its generation prompt. The validation step checks that required sections exist.

#### .loopr/ Exclusion

The `.loopr/` directory is already excluded from git via `.git/info/exclude` (existing behavior in `worktree::manager::ensure_loopr_excludes`). Run directories are orchestration state, not project code.

#### Cleanup

Run directories accumulate. Cleanup policy configured in `~/.config/loopr/loopr.yml`:

```yaml
runs:
  max-count: 10       # keep last N runs
  max-age-days: 30    # delete runs older than N days
```

Implementation deferred - noted as future work.

### The Decomposer

#### What It Is

A system call (function), not an agent. It does not have a session, FSM, or iteration loop. It takes a document, calls an LLM, validates the output, writes child .md files, creates child Doc records, and returns.

```rust
pub fn decompose(
    doc: &Doc,
    stores: &Stores,
    run_dir: &Path,
    config: &DecomposerConfig,
) -> eyre::Result<Vec<Doc>>
```

#### How It Works: Level-by-Level with Ratification

```
1. Plan activates
2. decompose(plan) -> produces N spec .md files + Doc records (in staging)
   - Validate each spec against spec template
   - Cycle detection on spec dependencies
   - On validation failure: error + retry once, then fail
3. For each spec: decompose(spec) -> produces N phase .md files + Doc records (in staging)
   - Validate each phase
   - Cycle detection on phase dependencies
4. For each phase: decompose(phase) -> produces N work .md files + Doc records (in staging)
   - Validate each work
   - Cycle detection on work dependencies
   - Extract acceptance_criteria into Doc struct
5. Hierarchical ratification (bottom-up):
   a. For each Phase: ratify its Works against the Phase (duplication, gaps, conflicts)
   b. For each Spec: ratify its Phases against the Spec
   c. Ratify all Specs against the Plan
   - Each ratification is one LLM call with bounded context (parent + its direct children only)
   - If ratification fails: identify which children have the problem, re-decompose that level
6. Flush staging to run directory and TaskStore
7. All docs transition to Active
8. Hand hierarchy to Coordinator for execution
```

**Why hierarchical ratification:** A naive "dump all docs into one prompt" blows out the context window for any non-trivial project (1 Plan + 3 Specs + 10 Phases + 50 Works = 64 documents). Hierarchical map-reduce bounds each ratification call to one parent and its direct children, which is always a manageable context size.

#### Validation

Each decomposition level is validated before proceeding to the next:

- **Template adherence:** Required sections present (from `docs/templates/`). Uses a separate `.pmt` file for the validation prompt. Uses Haiku model (structural check, not reasoning).
- **Dependency cycle detection:** After generating child docs, run a deterministic topological sort over the dependency graph. If a cycle is detected (Work A depends on Work B, Work B depends on Work A), validation fails immediately. The cycle is reported to the LLM as an error for retry. This is not an LLM check - it's a graph algorithm. No LLM can be trusted to generate a valid DAG on the first try.
- **Acceptance criteria extraction:** Parse the `## Acceptance Criteria` section from each .md file and populate the `acceptance_criteria: Vec<String>` field on the Doc struct. If the section is missing or empty for a Work doc, validation fails.
- On failure: supply the error text + original ask back to the LLM. One retry. If retry fails, halt with error.

#### Staging for Transactional Integrity

TaskStore is backed by JSONL files with no multi-record transactions. A daemon crash mid-decomposition would leave orphaned .md files and partial records.

The Decomposer writes to a staging area first:

1. Child .md files are written to a temporary directory (e.g., `.loopr/staging/`)
2. Doc records are accumulated in memory (not yet appended to TaskStore)
3. Validation and cycle detection run against the staged artifacts
4. If everything passes: flush .md files to the run directory and append Doc records to TaskStore
5. If anything fails: delete the staging directory. No records written. Clean state.

This makes decomposition atomic at the sub-graph level. Either all children of a parent are created, or none are.

#### Re-decomposition

During execution, the Coordinator may discover that a phase or work item doesn't work. The Coordinator calls the Decomposer targeting that specific level:

```rust
// "This phase broke. Give me new works for it."
let new_works = decompose(&broken_phase_doc, stores, run_dir, config)?;
```

The re-decomposition sequence:

1. Coordinator cancels active agents (implementers) on the old works
2. Coordinator triggers worktree cleanup for abandoned works (delete branches, remove worktree directories)
3. Coordinator abandons old work Doc records AND deletes their .md files
4. Coordinator abandons any Bundles associated with the old works
5. Decomposer generates new works (through staging, validation, cycle detection)
6. New works are Active and ready for assignment

The Decomposer decomposes from the target level downward only - it does not re-decompose the entire tree. The worktree cleanup is critical - abandoning a Doc record without severing the associated git branch and worktree leaves garbage in the repo.

#### Configuration

```rust
pub struct DecomposerConfig {
    pub provider: String,
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub validation_model: String,  // Haiku for template checks
}
```

In `~/.config/loopr/loopr.yml`:

```yaml
decomposer:
  provider: anthropic
  model: claude-sonnet-4-6
  api-key-env: ANTHROPIC_API_KEY
  max-tokens: 4096
  temperature: 0.3
  validation-model: claude-haiku-4-5-20251001
```

### Coordinator Scope Reduction

The Coordinator's `Planning` FSM state changes meaning:

**Before:** Coordinator calls LLM to generate Plan/Spec/Phase/Work one level at a time across multiple iterations, with validation and coverage evaluation loops.

**After:** The Decomposer runs to completion BEFORE the Coordinator starts. The activation sequence is:

1. Plan is activated (Draft -> Active)
2. Decomposer runs synchronously: Plan -> Specs -> Phases -> Works (with validation and ratification)
3. All child Docs are Active when the Decomposer finishes
4. Coordinator agent starts with a fully decomposed hierarchy
5. Coordinator enters at `ActivatePhase` (Full mode) or `Executing` (Brief mode)

The `Planning` FSM state on the Coordinator becomes a no-op or is removed. The Coordinator never enters it because decomposition is complete before the Coordinator is started. The `Interviewing` state remains for `InterviewMode::Auto` in headless contexts (separate concern, separate decision).

The `build_generation_footer`, `determine_generation_level`, and generation prompt builders in `src/agents/generation.rs` are replaced by the Decomposer. The Coordinator keeps its execution-phase logic: `ActivatePhase`, `Executing`, `PhaseGate`, `GoalComplete`, plus the retry/SLA/lifeguard machinery.

The Coordinator retains the ability to call the Decomposer for targeted re-decomposition during execution (when a phase or work fails and needs restructuring).

#### Tier (Full vs Brief)

The Decomposer determines tier, not the Coordinator. When the Decomposer reads the plan .md, it decides:

- **Full mode:** Plan defines or modifies contracts (data model, API, interfaces) -> decompose into Specs, Phases, Works
- **Brief mode:** No contract changes -> decompose directly into Works (skip Spec and Phase)

The existing tier-gate LLM classification is moved into the Decomposer as its first step. The `Tier` field moves off the Plan struct (it was on `Plan.tier`) - the Decomposer makes this decision and acts on it. The Coordinator doesn't need to know the tier; it just executes whatever hierarchy exists.

### Dependencies Replace Order

`Phase.order: u32` is removed. All levels use `dependencies: Vec<String>` (already present on Work).

The dependency contract:
- Dependencies are within-level only (Phase depends on Phase, Work depends on Work)
- Cross-level ordering is implicit: a Phase's Works don't start until the Phase is Active, and a Phase doesn't activate until its dependency Phases are Complete
- Linear sequencing: Phase B depends on Phase A. Phase C depends on Phase B.
- Parallel execution: Phase A and Phase B have no dependency between them.
- Diamond: Phase C depends on both Phase A and Phase B.

The Coordinator's `find_next_phase_to_activate` changes from iterating by `order` to finding Phases whose dependencies are all Complete.

### Implementation Plan

**Phase 1: Doc struct and file storage**

1. Create `src/domain/doc.rs` with `DocKind`, `Doc`, and the newtype wrappers
2. Implement `Record` for `Doc` (collection name: `"docs"`)
3. Add `markdown` field, `dependencies` field
4. Create run directory management (create, path resolution, slug generation)
5. Write .md file creation helper that slugifies titles and writes to run dir
6. Migrate handlers: `plan.create`, `spec.create`, `phase.create`, `work.create` -> `doc.create` (or keep separate handlers that construct Doc with the right kind)
7. Update `DaemonContext` hydration to load Docs
8. Tests: Doc CRUD, file writing, slug collision, serde roundtrip

**Phase 2: Decomposer**

1. Create `src/decomposer.rs` (or `src/decomposer/mod.rs`)
2. Implement `decompose()` function: read parent .md, build prompt with template, call LLM, parse output, validate, write child .md files, create Doc records
3. Implement validation step: template adherence check via Haiku
4. Implement ratification step: all-docs cross-check
5. Create decomposer `.pmt` files (one per level + validation + ratification)
6. Wire into plan activation: `accept_plan` -> write plan.md -> Decomposer -> Coordinator
7. Tests: decompose with MockLlm, validation failure + retry, ratification

**Phase 3: Coordinator scope reduction**

1. Remove generation logic from Coordinator (`build_generation_footer`, `determine_generation_level`, generation prompt builders)
2. Coordinator starts at `ActivatePhase` or `Executing` (hierarchy already decomposed)
3. Add re-decomposition call: Coordinator can invoke Decomposer for targeted rebuilds
4. Update `check_fsm_transition` to reflect new Planning state semantics
5. Tests: Coordinator with pre-decomposed hierarchy, re-decomposition trigger

**Phase 4: Plan entry paths**

1. Chat funnel `/accept`: writes plan.md, creates Doc record, triggers Decomposer, starts Coordinator
2. E2E test path: injects plan.md directly, same pipeline from Decomposer onward
3. Manifest path: injects pre-decomposed hierarchy, skips Decomposer, starts Coordinator
4. Tests: all three paths end-to-end

## Alternatives Considered

### Alternative 1: Keep Four Separate Structs

- **Description:** Maintain Plan, Spec, Phase, Work as distinct types with their own handlers, but add file storage
- **Pros:** No migration of existing handler code. Type safety without wrappers.
- **Cons:** Four copies of identical handler logic. Four Record impls. Four sets of CRUD tests. The structs are already identical except for Work's extra fields.
- **Why not chosen:** The duplication is the problem. A unified type with wrappers gives type safety AND eliminates the duplication.

### Alternative 2: Decomposer as an Agent

- **Description:** The Decomposer runs as a long-lived agent with its own session, FSM, and iteration loop
- **Pros:** Can handle complex multi-turn decomposition. Fits the existing agent infrastructure.
- **Cons:** Decomposition is a request-response operation, not a long-running loop. An agent session adds overhead (session management, cancellation, status tracking) for what is fundamentally "call LLM, validate, write files." The existing Coordinator-as-decomposer already proves that an agent loop is the wrong abstraction - most iterations are wasted on FSM bookkeeping, not decomposition.
- **Why not chosen:** A system call is simpler, testable, and composable. The Coordinator can call it when needed without spawning a session.

### Alternative 3: Content Stays in JSON Fields

- **Description:** Keep artifact content in `description: String` on the struct, don't write .md files
- **Pros:** No file I/O. No slug management. No run directories.
- **Cons:** Content is invisible without tooling. Not editable. Not diffable. No template enforcement. Agents can't read artifacts the same way they read code files. The LLM must receive content via JSON actions rather than reading files.
- **Why not chosen:** The artifacts ARE documents. They should be documents on disk.

### Alternative 4: Nested Directory Structure

- **Description:** Mirror the hierarchy in directories: `specs/01-core/phases/01-data/works/01-schema.md`
- **Pros:** Hierarchy visible in filesystem.
- **Cons:** Deep nesting. Directory structure duplicates the parent-child relationship already in Doc records. Renaming a spec means renaming phase and work directories too. Annoying to navigate.
- **Why not chosen:** Flat files in a run directory with type-prefixed slugs are simpler. The hierarchy is in the records, not the filesystem.

### Alternative 5: TaskStore IDs as Filenames

- **Description:** Use `sp-b2c1.md` instead of `spec-core-implementation.md`
- **Pros:** Perfect 1:1 mapping to TaskStore. Zero collision risk.
- **Cons:** `ls` is meaningless. Can't tell what a file is about without opening it.
- **Why not chosen:** Slugified titles are human-readable. The record's `markdown` field provides the mapping.

## Technical Considerations

### Dependencies

- No new crates. `Doc` uses existing TaskStore `Record` trait.
- The Decomposer uses existing `ureq` (sync HTTP client) or `reqwest` depending on calling context.
- Existing `.pmt` prompt file infrastructure is reused for decomposer prompts.

### Performance

- File I/O for reading .md files during prompt building: one `fs::read_to_string` per file. Trivial.
- Decomposition runs once per plan activation (not per Coordinator iteration). LLM calls are the bottleneck, not file I/O.
- Ratification is one additional LLM call after decomposition completes.

### Security

- .md files are written to `.loopr/runs/` which is git-excluded. No risk of committing orchestration state.
- Slug generation must sanitize filenames (no path traversal, no special characters).

### Testing Strategy

- Unit tests: Doc CRUD, DocKind validation, slug generation, dependency resolution
- Unit tests: Decomposer with MockLlm - validate output structure, retry on validation failure, ratification
- Integration tests: full plan entry -> decomposition -> Coordinator execution with MockLlm
- E2E tests: inject plan.md, verify decomposition produces expected hierarchy, Coordinator executes
- Existing Coordinator tests updated: pre-decomposed hierarchy as input (no generation logic)

### Rollout Plan

Phase 1 (Doc + files) can land independently - it's a data model change.
Phase 2 (Decomposer) depends on Phase 1.
Phase 3 (Coordinator reduction) depends on Phase 2.
Phase 4 (entry paths) depends on all prior phases.

Each phase passes `otto ci` independently.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Doc migration breaks existing handlers | Medium | High | Phased rollout. Each phase passes `otto ci`. |
| Decomposer LLM output doesn't match template | High | Medium | Validation + one retry + halt. Templates are explicit. Haiku checks structure. |
| Ratification step adds too much latency | Low | Low | One LLM call. Can use Haiku. Can be disabled in config for speed. |
| Slug collisions in filenames | Low | Low | Append `-2`. Log warning. IDs are unique regardless. |
| Work still needs richer status FSM than other levels | High | Medium | See Open Questions. WorkStatus may stay on the Work wrapper. |
| Re-decomposition during execution creates orphaned files | Medium | Low | Decomposer abandons old Doc records AND deletes old .md files. |

## Edge Cases

### Decomposer fails partway through

The Decomposer produced specs but fails on phases (LLM error, validation failure after retry).

**Behavior:** Staging prevents partial state. Each `decompose()` call writes to staging first. If phase decomposition fails for a spec, the staging directory is deleted - no .md files written to the run directory, no Doc records appended to TaskStore. The successfully decomposed specs remain. The Decomposer can be retried from the spec level. The user sees an error: "Decomposition failed at phase level for spec-core-implementation. Retry or revise the plan."

### Re-decomposition during execution

The Coordinator discovers a phase is failing and calls the Decomposer to re-generate its works. But an implementer is mid-flight on one of the old works, with an active worktree and in-progress git branch.

**Behavior:** Full cleanup before re-decomposition: (1) cancel active agents on old works, (2) clean up worktrees and git branches for those works, (3) abandon associated Bundles, (4) abandon old work Doc records and delete their .md files, (5) call Decomposer for new works (through staging), (6) new works are Active and ready for assignment. The worktree cleanup is mandatory - without it, abandoned branches and worktree directories accumulate as garbage.

### Dependency cycle generated by LLM

The Decomposer generates works where Work A depends on Work B and Work B depends on Work A.

**Behavior:** The cycle detection step (topological sort) catches this deterministically before any records are written. The cycle is reported as a validation error. The error text (including which IDs form the cycle) is sent back to the LLM with the original ask for one retry. If the retry also produces a cycle, decomposition halts.

### Migration from current data model

Existing TaskStore data uses separate `plans`, `specs`, `phases`, `works` collections. The new model uses a single `docs` collection.

**Behavior:** Phase 1 of implementation creates the new `docs` collection alongside the old ones. A migration utility reads old collections and creates equivalent Doc records. Old collections are not deleted until the migration is verified. Tests run against both old and new models during transition.

## Open Questions

- [x] **WorkStatus vs HierarchyStatus:** RESOLVED - Doc has no status. Each wrapper owns its own status enum. Plan/Spec/Phase use `HierarchyStatus`. Work uses `WorkStatus`. Compiler enforces it.
- [x] **Acceptance criteria as a struct field:** RESOLVED - `acceptance_criteria: Vec<String>` is a field on Doc. Authored in the .md file, extracted into the struct by the Decomposer at creation time. Validators and agents check it programmatically.
- [x] **Draft status:** RESOLVED - Keep Draft. Revisit if it proves unnecessary in practice.
- [x] **Run directory storage location:** RESOLVED - `.loopr/runs/` in the project root. Per-project, local.
- [x] **Cleanup policy:** RESOLVED - Default 21 days. Override in `~/.config/loopr/loopr.yml` via `runs: { max-age-days: 21 }`.

## References

- `docs/2026-04-04-chat-tunnel-vs-e2e-insertion-for-entering-a-plan.md` - How plans enter the system (authoritative)
- `docs/design/2026-03-04-tui-chat-plan-funnel.md` - Chat funnel design
- `docs/design/2026-03-17-chat-to-orchestration-bridge.md` - Bridge design
- `docs/design/2026-02-25-orchestration-spine.md` - Original architecture
- `docs/design/2026-02-26-multi-level-rwl.md` - Coordinator design
- `docs/templates/plan.md` - Plan template
- `docs/templates/spec.md` - Spec template
- `docs/templates/phase.md` - Phase template
- `docs/templates/work.md` - Work template
