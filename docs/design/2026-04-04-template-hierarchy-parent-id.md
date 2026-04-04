# Design Document: Template Hierarchy and Parent-ID Refactor

**Author:** Scott A. Idler
**Date:** 2026-04-04
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Unify the typed parent fields (`plan_id`, `spec_id`, `phase_id`) into a
single `parent_id: String` across Spec, Phase, and Work domain models.
This unlocks a two-tier lifecycle (Full and Brief) where the Coordinator
can skip Spec and Phase for contract-neutral work, pointing Work records
directly at a Plan. The template hierarchy in `docs/templates/` has
already been updated to match; this design doc covers the code changes
needed to realize it.

## Problem Statement

### Background

Loopr's orchestration hierarchy is Plan -> Spec -> Phase -> Work. Each
level is a TaskStore record with a typed parent field:

- `Spec.plan_id: String`
- `Phase.spec_id: String`
- `Work.phase_id: String`

The `docs/templates/` directory defines document templates for each level
with required and conditional sections, scaling guidance, and validation
rules.

### Problem

Three issues converge:

1. **Rigid parent fields block Brief mode.** A Work record's `phase_id`
   field can only point to a Phase. In Brief mode (Plan -> Work), there
   is no Phase - the Work's parent is a Plan. Storing a Plan ID in
   `phase_id` corrupts the semantic meaning of the field and breaks
   type-safe indexing.

2. **The integrator's coverage evaluator already works around this.** The
   `handle_integrator_coverage_evaluate` function in `integrator.rs`
   uses a generic `parent_collection + parent_id` pattern, then switches
   on `parent_collection` to filter by the typed field (`s.plan_id`,
   `p.spec_id`, `w.phase_id`). This switch statement is the cost of the
   current design - it must know which typed field to query for each
   collection.

3. **Small projects pay full ceremony tax.** Every project currently
   requires Plan -> Spec -> Phase -> Work, even when the work is a
   single-file fix or a repeated pattern (e.g., e2e targets). The
   templates support scaling down via "Brief mode" guidance, but the
   Coordinator FSM has no mechanism to skip Spec and Phase.

### Goals

- A Work record can point to a Phase (Full mode) or a Plan (Brief mode)
  using the same field
- The Coordinator FSM supports a Brief path: Plan -> Work, skipping Spec
  and Phase generation
- All parent-child queries use a single `parent_id` field - no switch
  statements on collection type
- Existing JSONL data continues to load via serde aliases
- Context escalation: failing Work items can walk up the `parent_id`
  chain for retry context

### Non-Goals

- Adding a parent level above Plan (Epics, Initiatives) - no speculative
  future-proofing
- Changing the CoverageReport or ValidationReport models - they already
  use generic `parent_id`
- Modifying the TaskStore crate itself - all changes are in Loopr's
  domain layer
- Changing the template file format or adding new template types

## Proposed Solution

### Overview

Replace typed parent fields with a uniform `parent_id: String` on Spec,
Phase, and Work. Plan has no parent field (it is the root). Add serde
aliases for backward compatibility with existing JSONL records. Update
all handlers, queries, and Coordinator logic to use `parent_id`. Add a
Brief mode gate to the Coordinator FSM.

### Data Model

#### Before

```rust
pub struct Spec {
    pub id: String,
    pub plan_id: String,    // typed: always points to Plan
    ...
}

pub struct Phase {
    pub id: String,
    pub spec_id: String,    // typed: always points to Spec
    ...
}

pub struct Work {
    pub id: String,
    pub phase_id: String,   // typed: always points to Phase
    ...
}
```

#### After

```rust
pub struct Plan {
    pub id: String,
    pub tier: Tier,         // Full or Brief, set on activation
    ...
}

pub struct Spec {
    pub id: String,
    pub parent_id: String,  // always points to Plan
    ...
}

pub struct Phase {
    pub id: String,
    pub parent_id: String,  // always points to Spec
    ...
}

pub struct Work {
    pub id: String,
    pub parent_id: String,  // points to Phase (Full) or Plan (Brief)
    ...
}
```

Clean break - no serde aliases, no backward compatibility with old
field names. Existing JSONL files with `plan_id`/`spec_id`/`phase_id`
will not load. This is acceptable because JSONL data is ephemeral
per-goal-run.

Plan gains a `tier: Tier` field. The tier is a property of the work,
not the run - a Plan that defines no contracts is always Brief.

#### Hierarchy traversal

The record type determines what the parent is:

| Record type | parent_id points to | Collection to query |
|-------------|--------------------|--------------------|
| Spec | Plan (always) | plans |
| Phase | Spec (always) | specs |
| Work | Phase (Full) or Plan (Brief) | phases or plans |

For Work, the ID prefix disambiguates: `ph-*` is a Phase, `pl-*` is a
Plan. The Coordinator already generates IDs with prefixes (`pl`, `sp`,
`ph`, `wk`).

### Constructor Changes

```rust
// Before
impl Spec {
    pub fn new(plan_id: String, title: String, description: String) -> Self { ... }
}

// After
impl Spec {
    pub fn new(parent_id: String, title: String, description: String) -> Self { ... }
}
```

Same pattern for Phase::new and Work::new - rename the parameter from
the typed name to `parent_id`.

### Index Changes

```rust
// Before (spec.rs)
fn indexed_fields(&self) -> HashMap<String, IndexValue> {
    let mut m = HashMap::new();
    m.insert("status".into(), IndexValue::String(self.status.to_string()));
    m.insert("plan_id".into(), IndexValue::String(self.plan_id.clone()));
    m
}

// After (spec.rs)
fn indexed_fields(&self) -> HashMap<String, IndexValue> {
    let mut m = HashMap::new();
    m.insert("status".into(), IndexValue::String(self.status.to_string()));
    m.insert("parent_id".into(), IndexValue::String(self.parent_id.clone()));
    m
}
```

Same for Phase and Work.

### Handler Changes

All create handlers (`spec.create`, `phase.create`, `work.create`)
currently extract a typed parameter (`plan_id`, `spec_id`, `phase_id`)
from the request. These change to `parent_id`:

```rust
// Before (handlers/spec.rs)
let plan_id = params.get("plan_id")...;
let parent = stores.read_plans().get(&plan_id)...;
Spec::new(plan_id, title, description)

// After (handlers/spec.rs)
let parent_id = params.get("parent_id")...;
let parent = stores.read_plans().get(&parent_id)...;
Spec::new(parent_id, title, description)
```

For `spec.create` and `phase.create`, the parent existence check
remains single-store - a Spec's parent is always a Plan, a Phase's
parent is always a Spec. The handler MUST reject mismatched parent
types (e.g., Phase with a Plan ID as parent).

For `work.create`, the handler must accept a parent that is either a
Phase or a Plan:

```rust
// After (handlers/work.rs)
let parent_id = params.get("parent_id")...;

// Try Phase first, then Plan (Brief mode)
let parent_exists = stores.read_phases().get(&parent_id).is_some()
    || stores.read_plans().get(&parent_id).is_some();
```

Duplicate detection also changes from filtering by `phase_id` to
filtering by `parent_id`.

### Integrator Coverage Evaluator

The switch statement in `handle_integrator_coverage_evaluate` simplifies:

```rust
// Before
let child_specs: Vec<_> = specs.values()
    .filter(|s| s.plan_id == parent_id).collect();
let child_phases: Vec<_> = phases.values()
    .filter(|p| p.spec_id == parent_id).collect();
let child_works: Vec<_> = works.values()
    .filter(|w| w.phase_id == parent_id).collect();

// After
let child_specs: Vec<_> = specs.values()
    .filter(|s| s.parent_id == parent_id).collect();
let child_phases: Vec<_> = phases.values()
    .filter(|p| p.parent_id == parent_id).collect();
let child_works: Vec<_> = works.values()
    .filter(|w| w.parent_id == parent_id).collect();
```

The field name is now uniform. The switch on `parent_collection` to pick
which collection to query remains - that logic is about which store to
read, not which field to filter on.

### Coordinator FSM: Brief Mode

#### Tier Gate

The tier is determined by an LLM call that reads the Plan and classifies
it as Full or Brief. This runs once when a Plan is activated, and the
result is stored on `Plan.tier`.

```rust
pub async fn determine_tier(plan: &Plan, client: &LlmClient) -> Tier {
    // Send Plan description + contracts section to a lightweight model
    // (haiku) with a curated classification prompt.
    // Returns Tier::Full or Tier::Brief.
}
```

The classification prompt lives in a versioned file (e.g.,
`docs/templates/tier-gate.pmt`) so it can be tuned without code changes.
The prompt instructs the model to look for:
- New data model definitions (struct/entity with field names and types)
- New API contracts (endpoints, function signatures, CLI commands)
- New public interfaces between modules
- References to shared specs ("Follows docs/specs/...") indicate Brief
- "No contract changes" language indicates Brief

Haiku is sufficient for this - it's a binary classification on a
structured document. Fast, cheap, and the prompt gives it explicit
criteria.

The tier is stored on Plan (persistent), not CoordinatorState
(ephemeral). A Plan that defines no contracts is always Brief regardless
of how many times it is run.

#### Brief Mode FSM Path

In Brief mode, the FSM skips the Spec and Phase generation levels:

```
Full:  Interviewing -> Planning -> ActivatePhase -> Executing -> PhaseGate -> GoalComplete
Brief: Interviewing -> Planning -> Executing -> GoalComplete
```

In Brief mode, `CoordinatorState.current_phase_id` stays `None`. The
sweep and completion logic use the Plan ID directly as the parent
target for filtering Works.

Changes to `determine_generation_level()`:

```rust
// Current logic (always Full):
// No Specs? -> Generate Spec
// No Phases? -> Generate Phase
// No Works? -> Generate Work

// Brief mode addition:
if tier == Tier::Brief {
    // Skip Spec and Phase generation
    // Go directly from Plan to Work generation
    if no_works_exist {
        return Some(GenerationLevel::Work);
    }
    return None;
}
```

Changes to `check_fsm_transition()`:

```rust
// Brief mode: skip ActivatePhase and PhaseGate
// Planning transitions directly to Executing when Works exist
// Executing transitions directly to GoalComplete when all Works are terminal
```

#### Status Bubbling in Brief Mode

In Full mode, Work completion bubbles: Work -> Phase -> Spec -> Plan.
In Brief mode, Work completion bubbles directly: Work -> Plan.

The `check_phase_completion()` function becomes
`check_parent_completion()` - it finds all Works where
`parent_id == current_parent_id` and checks if all are terminal.

### Context Escalation

When a Work item fails repeatedly, the retry supervisor walks the
`parent_id` chain to gather context:

1. Read the failing Work's `parent_id`
2. Load the parent record (Phase or Plan)
3. If Phase, continue up: Phase.parent_id -> Spec -> Plan
4. At each level, extract relevant context:
   - Phase: validation scope, deliverables
   - Spec: architectural decisions, failure modes, interfaces
   - Plan: contracts, requirements, acceptance criteria
5. Revise the Work's Implementation Notes and Constraints (retry rules
   4 and 5) using the gathered context
6. Hand the revised Work to a fresh implementer

This uses the existing `decomposition_attempts` tracking on
CoordinatorState, which already keys by `parent_id`.

### Template Alignment

The templates in `docs/templates/` have already been updated:

| Template | Parent section | Children section |
|----------|---------------|-----------------|
| plan.md | (root) | Specs (Full) / Work Items (Brief) |
| spec.md | Overview references Plan | Phases |
| phase.md | Parent (references Spec) | Work Items |
| work.md | Parent (references Phase or Plan) | (leaf) |

`sections.yml` has `brief` flags on each section and `parent`/`children`
metadata on each document type.

## Implementation Plan

### Phase 1: parent_id Refactor (Foundation)

Rename typed parent fields to `parent_id` with serde aliases. All
existing behavior preserved - no Brief mode yet.

**Files:**
- `src/domain/spec.rs` - field rename, constructor, indexed_fields
- `src/domain/phase.rs` - field rename, constructor, indexed_fields
- `src/domain/work.rs` - field rename, constructor, indexed_fields
- `src/daemon/handlers/spec.rs` - param rename, parent validation
- `src/daemon/handlers/phase.rs` - param rename, parent validation
- `src/daemon/handlers/work.rs` - param rename, parent validation,
  duplicate detection
- `src/daemon/handlers/integrator.rs` - filter field rename
- `src/agents/executor/action/record.rs` - param rename in create
  handlers
- `src/agents/executor/action/work.rs` - param rename
- `src/agents/generation.rs` - filter field rename in coverage checks
- `src/agents/coordinator.rs` - filter field rename in phase completion,
  sweep, build_phase_status
- `src/agents/coordinator/run.rs` - any direct field access
- `src/daemon/handlers/common.rs` - test helper functions
- `tests/funnel.rs` - integration tests referencing typed parent fields
- Prompt template files (`.pmt`) that instruct the LLM to provide
  `plan_id`/`spec_id`/`phase_id` as action parameters - must say
  `parent_id` instead

**Validation:** `otto ci` passes. Existing JSONL files load via aliases.
Serde roundtrip tests verify both old and new field names deserialize.

### Phase 2: Brief Mode in Coordinator

Add tier gate and Brief FSM path.

**Files:**
- `src/domain/plan.rs` - add `tier: Tier` field to Plan struct
- `src/agents/generation.rs` - `determine_generation_level()` respects
  tier, add `determine_tier()` LLM classification call
- `src/agents/coordinator.rs` - `check_fsm_transition()` Brief path,
  `check_parent_completion()` replaces `check_phase_completion()`
- `src/agents/coordinator/run.rs` - Brief mode sweep logic
- `src/daemon/handlers/work.rs` - accept Plan as parent in Brief mode
- `src/daemon/handlers/plan.rs` or `coordinator.rs` - call
  `determine_tier()` on plan activation, store result on Plan
- `docs/templates/tier-gate.pmt` - new classification prompt

**Validation:** `otto ci` passes. New tests verify Brief mode FSM path:
Plan created -> Work created with Plan as parent -> Work completes ->
GoalComplete.

### Phase 3: Context Escalation

Walk `parent_id` chain for retry context.

**Files:**
- `src/agents/coordinator.rs` - retry supervisor logic
- `src/agents/generation.rs` - context gathering from parent chain

**Validation:** `otto ci` passes. Test: Work fails -> parent chain
walked -> revised Work created with updated Implementation Notes.

## Alternatives Considered

### Alternative 1: Keep typed fields, add plan_id to Work

- **Description:** Add `plan_id: Option<String>` to Work alongside
  `phase_id`. In Brief mode, populate `plan_id` instead of `phase_id`.
- **Pros:** No rename, no migration, type-safe
- **Cons:** Two fields for the same concept. Every query must check both.
  Every new hierarchy level requires a new field on every downstream
  record. The integrator switch statement gets worse, not better.
- **Why not chosen:** Violates naming consistency rule. The parent
  relationship is one concept - it should be one field.

### Alternative 2: Generic parent_id + parent_collection (like CoverageReport)

- **Description:** Add both `parent_id: String` and
  `parent_collection: String` to every record.
- **Pros:** Fully generic, any record can parent any other
- **Cons:** Over-engineered. Spec's parent is always a Plan. Phase's
  parent is always a Spec. Only Work has variable parent type, and the
  ID prefix already disambiguates. Adding `parent_collection` to records
  where the collection is fixed by the record type is redundant data.
- **Why not chosen:** The record type already implies the parent
  collection for Spec and Phase. For Work, the ID prefix (`pl-*` vs
  `ph-*`) is sufficient.

### Alternative 3: Polymorphic enum parent field

- **Description:** `parent: ParentRef` where `ParentRef` is an enum
  with `Plan(String)`, `Spec(String)`, `Phase(String)` variants.
- **Pros:** Type-safe, self-documenting, no ambiguity
- **Cons:** Breaks JSONL format significantly. Serde representation
  would be `{"parent": {"Phase": "ph-123"}}` instead of a flat string.
  Every query must destructure the enum. No serde alias path for
  backward compatibility.
- **Why not chosen:** JSONL format break with no clean migration path.

## Technical Considerations

### Dependencies

No new dependencies. All changes use existing serde, taskstore, and
loopr infrastructure.

### Performance

No performance impact. The indexed field rename is a key name change -
same number of indexes, same query patterns.

### JSONL Backward Compatibility

Clean break. No serde aliases. Existing JSONL files with old field
names (`plan_id`, `spec_id`, `phase_id`) will fail to deserialize.
This is acceptable - JSONL data is ephemeral per-goal-run. Any in-flight
goals should be completed or abandoned before deploying this change.

### Testing Strategy

1. **Serde roundtrip tests:** verify both old field names (`plan_id`,
   `spec_id`, `phase_id`) and new (`parent_id`) deserialize correctly
2. **Indexed field tests:** verify `parent_id` key appears in
   `indexed_fields()` output
3. **Handler tests:** verify create/get handlers use `parent_id` param
4. **Brief mode FSM tests:** Plan -> Work -> GoalComplete path
5. **Context escalation tests:** Work failure -> parent chain traversal
6. **Integration test:** Brief mode e2e - create Plan, create Work with
   Plan as parent, complete Work, verify GoalComplete

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Missed field rename causes runtime deserialization failure | Medium | High | Grep for plan_id, spec_id, phase_id across entire codebase after refactor. `otto ci` catches compile errors. Clean break on JSONL - old data does not load. |
| Brief mode FSM path has untested edge cases | Medium | Medium | Dedicated test suite for Brief mode. Start with e2e targets as real-world validation. |
| ID prefix disambiguation is fragile | Low | Medium | Prefixes are generated by `id::generate_id()` which is stable. Add a helper function `parent_collection_for(id: &str) -> &str` that centralizes the prefix logic. |
| Tier classification prompt drifts from actual contract detection needs | Low | Medium | Prompt is versioned in docs/templates/tier-gate.pmt. Review and tune as false positives/negatives emerge from real usage. |
| Brief tier misjudged - work actually needs Spec | Low | Medium | Allow tier upgrade: if the Coordinator detects contract changes during Planning, escalate from Brief to Full. Not required for initial implementation. |
| Prompt templates (.pmt) still reference old field names | Medium | High | Include .pmt files in the Phase 1 grep sweep. Agent actions fail if the LLM provides wrong parameter names. |

## Resolved Questions

- [x] **Tier storage:** Stored on `Plan.tier` (persistent). The tier is
  a property of the work, not the run.
- [x] **ID prefix helper:** Standalone function `parent_collection_for(id: &str)`.
  Document in a large docstring that this could be promoted to a typed
  ID wrapper (`RecordId { prefix, value }`) if prefix logic grows, but
  a 5-line function is sufficient for now.
- [x] **Tier detection:** LLM classification call using Haiku with a
  curated prompt in `docs/templates/tier-gate.pmt`. Prompt enumerates
  what to look for (data models, API contracts, shared spec references).
  Tunable without code changes.

## Resolved Questions (continued)

- [x] **User override:** Yes. If the user explicitly says Full or Brief
  during the interview, that takes precedence over the LLM classification.
  Precedence: user explicit > LLM classification > default.
- [x] **Classification fallback:** Default to Full. Full is the safe
  path - it generates all documents. Brief is an optimization that skips
  levels. When in doubt, don't skip.
