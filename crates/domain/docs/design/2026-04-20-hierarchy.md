# Design Document: Stage 6 Hierarchy — `Work` Record and `WorkStatus` FSM

**Author:** Claude (with Scott)
**Date:** 2026-04-20
**Status:** Implemented
**Review Passes Completed:** 5/5 + Architect rounds 1 and 2
**Scope gate:** [`docs/design/2026-04-20-stage-6-scope.md`](../../../../docs/design/2026-04-20-stage-6-scope.md) — decisions D1–D11, A+1..3, U+1..5 are locked there; this doc references the matrix by row rather than re-litigating.

## Summary

Introduce the `Work` record type and its 10-state `WorkStatus` FSM into the
`domain` crate, plus an `AcceptanceCriteria` newtype shared by any future
record kind that needs a list of assertions. All FSM and persistence
behavior is implemented via the existing `#[derive(Fsm)]` and
`#[derive(Record)]` macros so this change is overwhelmingly mechanical:
the architectural decisions (10 states, typed `parent_id`, deferred fields,
birth-state) are already resolved in the scope memo and the body of this
doc locks the concrete shape.

## Problem Statement

### Background

Stage 5 shipped the `Plan` record, its `PlanStatus` FSM, the `PlanId` typed
newtype, and the `Store` / `PlansStore` wrappers (v0.5.11–v0.5.16). The
daemon now accepts `plan.create` / `plan.list` IPC calls and persists Plans
to `.loopr/taskstore/plans.jsonl`. A `loopr plan "x"` invocation returns the
created Plan's id and exits; the Plan remains in `PlanStatus::Active` with
no children.

Stage 6's goal is for `loopr plan "x"` to decompose that Plan into at least
one `Work` record persisted to `.loopr/taskstore/works.jsonl` with
dependencies on the Plan. Before the decomposer itself can be designed
(upcoming `plan-then-decompose.md`) or the `LlmClient` trait written
(upcoming `llm-client.md`), the `Work` type must exist.

### Problem

`crates/domain/src/` defines `Plan` / `PlanStatus` / `PlanId` but no `Work`
sibling. Stage 6 code has no Rust type to construct, persist, or transition.

### Goals

- `Work` record type in `crates/domain/` with the field set v3 evolved through
  v0.1.96 (post-description-crisis), ported verbatim except for one deletion
  and one typing upgrade.
- `WorkStatus` enum with 10 variants and a complete FSM transition + override
  table, driven by `#[derive(Fsm)]` the same way `PlanStatus` is.
- `WorkId` typed newtype via the existing `id_type!(WorkId, "wk")` macro.
- `AcceptanceCriteria` newtype suitable for reuse by `Plan` and by later
  stages (`Spec`/`Phase` if they ever land).
- `Record` derive so `PlansStore`-style typed accessors are possible in
  `store` at Stage 6's persistence step (handled by the follow-up
  `plan-then-decompose.md`).
- Unit + integration test coverage matching `Plan`'s shape (FSM transition
  sweep, override sweep, serde round-trip, `Record` trait, AC newtype).

### Non-Goals

- `Spec`, `Phase`, `Tick`, `Bundle` records — deferred.
- `blocked_reason: Option<BlockedReason>` on `Work` — v4 addition, deferred
  per scope memo D3. FSM can still reach `Blocked`; the reason field arrives
  when Stage 7's coordinator needs it.
- FSM guards (`deps-ready: pending → ready`, `no-sessions-on-done:
  integrated → done`). v4's `work.yml` carried these; v5's `#[derive(Fsm)]`
  does not accept a `guards(...)` clause. Both guards are Stage 7
  Coordinator concerns. Session state lives in `agents` + the
  daemon loop; the `integrator` crate cannot see it and is not
  authorized on the `Integrated => Done` edge. Enforce both guards
  in Coordinator pre-checks when Stage 7 writes those transitions;
  do not extend the macro now.
- `Plan.decomposition_attempts` / `Plan.bubble_up_count` — v3 fields that
  track decomposer retry counts. Stage 6's exit criterion does not require
  them; Stage 7's reactive coordinator does. Defer.
- Any decomposer logic, LLM call, or prompt. This doc is domain-only.
- Markdown emission (`.loopr/docs/<id>.md`). Per scope memo D5, skip.

## Proposed Solution

### Overview

Follow the exact pattern already proven by `Plan` / `PlanStatus` / `PlanId`:

- Typed id via `id_type!(WorkId, "wk")`.
- `AcceptanceCriteria` as a thin newtype around `Vec<String>` in its own
  file (`crates/domain/src/criteria.rs`), matching v3's layout — grows later
  without churning `work.rs`.
- `WorkStatus` as a unit enum with per-variant `#[transitions(...)]` /
  `#[overrides(...)]` attributes consumed by `#[derive(Fsm)]`.
- `Work` as a `#[derive(Record)]` struct with `parent_id: PlanId`,
  `status: WorkStatus`, and the ports of v3's unused-but-load-bearing
  fields (`attempt_count`, `session_failure_count`, `files`, `assignee`).

No new derive macros. No new crate dependencies. No `async_trait`. No
`eyre::Report` leakage at this layer (FSM errors return the typed
`FsmError<WorkStatus>` the existing macro emits).

### Architecture

```
crates/domain/
├── src/
│   ├── lib.rs          — re-export Work, WorkStatus, WorkId, AcceptanceCriteria
│   ├── criteria.rs     — AcceptanceCriteria newtype (NEW)
│   ├── work.rs         — Work struct + WorkStatus enum (NEW)
│   ├── plan.rs         — existing, unchanged
│   ├── fsm.rs          — existing
│   ├── id.rs           — existing; gains `id_type!(WorkId, "wk")`
│   ├── role.rs         — existing
│   └── id/tests.rs     — existing, gains a WorkId smoke block
└── tests/
    └── work.rs         — NEW integration tests (FSM sweep, Record, serde,
                          AC round-trip). Mirrors the existing tests/plan.rs.
```

Tests live at `crates/domain/tests/work.rs` (integration test, black-box
view of the crate's public API), matching `tests/plan.rs`'s convention. No
sibling-module `src/work/tests.rs` block — `Plan` does not have one and
this doc preserves the per-record-kind symmetry. `AcceptanceCriteria` is
tested inside `tests/work.rs` rather than a separate file; it's small,
and pairing the AC tests with the first consumer keeps them legible.
Per the `rules/rust.md` "tests in their own files" rule we would extract
if either file crosses ~200 lines; both should stay comfortably under.

### Data Model

#### `AcceptanceCriteria` (new, `criteria.rs`)

```rust
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AcceptanceCriteria(pub Vec<String>);

impl AcceptanceCriteria {
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
    pub fn len(&self) -> usize { self.0.len() }
    pub fn iter(&self) -> std::slice::Iter<'_, String> { self.0.iter() }
}
```

`#[serde(transparent)]` so the wire form is `["criterion-1", "criterion-2"]`
not `{"0": [...]}`. The inner `Vec<String>` is `pub`, so the decomposer
constructs an `AcceptanceCriteria` directly (`AcceptanceCriteria(vec)`)
once the LLM's tool-use response is parsed. No `From<Vec<String>>` impl
needed; add it when a call site without struct-literal access appears.

#### `WorkId` (new, added to `id.rs`)

```rust
id_type!(WorkId, "wk");
```

One line. Same macro that produced `PlanId`. Gives `WorkId::new()`,
`AsRef<str>`, `Display`, `FromStr<Err = Infallible>`, serde transparent,
Hash/Eq/Clone.

#### `WorkStatus` (new, `work.rs`)

Ten variants, all unit. The decision for 10-not-9 is scope memo D2: match
`v5 PlanStatus` symmetry (which already carries `Superseded` as a terminal).

FSM transitions and overrides are expressed as per-variant attributes. The
body is a direct port of v4's `resources/engine/fsm/work.yml`, with v3's
9-state shape as a sanity baseline:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, Fsm)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[fsm(
    role = crate::Role,
    terminal = [Done, Superseded, Abandoned],
    transitions(
        Draft       => Pending    by (Coordinator),
        Draft       => Ready      by (Coordinator),
        Draft       => Superseded by (Coordinator, Director),
        Draft       => Abandoned  by (Coordinator, Director),
        Pending     => Ready      by (Coordinator),
        Pending     => Superseded by (Coordinator, Director),
        Pending     => Abandoned  by (Coordinator, Director),
        Ready       => InProgress by (Coordinator),
        Ready       => Blocked    by (Coordinator),
        Ready       => Superseded by (Coordinator, Director),
        Ready       => Abandoned  by (Coordinator, Director),
        InProgress  => Blocked    by (Coordinator, Implementer),
        InProgress  => InReview   by (Implementer),
        InProgress  => Superseded by (Coordinator, Director),
        InProgress  => Abandoned  by (Coordinator, Director),
        Blocked     => Ready      by (Coordinator),
        Blocked     => Superseded by (Coordinator, Director),
        Blocked     => Abandoned  by (Coordinator, Director),
        InReview    => InProgress by (Coordinator),
        InReview    => Integrated by (Integrator),
        InReview    => Superseded by (Coordinator, Director),
        InReview    => Abandoned  by (Coordinator, Director),
        Integrated  => Done       by (Coordinator),
        Integrated  => Superseded by (Coordinator, Director),
        Integrated  => Abandoned  by (Coordinator, Director),
    ),
    overrides(
        Ready      => Done     by (Coordinator),
        InProgress => Ready    by (Coordinator),
        InProgress => InReview by (Coordinator),
        InReview   => Ready    by (Coordinator),
    ),
)]
pub enum WorkStatus {
    Draft,
    Pending,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Integrated,
    Done,
    Superseded,
    Abandoned,
}
```

Invariants the macro enforces at compile time:
- Every target variant listed in `transitions(...)` / `overrides(...)` must
  be a variant of the enum.
- Terminal variants may not appear on the left-hand side of any transition.
- No duplicate edge (same `from => to` pair) in `transitions(...)`.
- `overrides(...)` cannot duplicate an edge already in `transitions(...)`.

Notable edges (semantics not obvious from the table alone):

- `Ready => Done by (Coordinator)` — the "no-op Work" bypass, lives
  in the `overrides(...)` block, not `transitions(...)`. Valid only
  when (a) the Work was authored but overtaken before it ran and the
  AC is already satisfied by prior commits, or (b) in test/recovery-
  bootstrap paths. Living in `overrides(...)` is the structural
  enforcement of "no AC skipping on the normal path": callers must
  reach for `Work::override_status(Done, Coordinator)` explicitly,
  and a stray `Work::transition(Done, Coordinator)` from `Ready` is a
  typed error, not a silent shortcut. v3 and v4 carried the edge in
  their routine tables; v5 demotes it to override on Architect round-2
  guidance so the bypass is grep-able and audit-able at call sites.
- `Integrated => Superseded by (Coordinator, Director)` — cascade-
  cancellation edge. Present so a `Plan: Active -> Superseded` can
  cascade to every Work regardless of state, including Works
  mid-integration, without failing `FsmError::NoTransition`. Absent
  from v4's `work.yml`; the Architect's round-2 review flagged the
  gap.
- `InProgress => Blocked by (Coordinator, Implementer)` — the
  Implementer's "I am stuck" authority. The Implementer cannot
  unassign itself back to `Ready` mid-work; that path exists only in
  the overrides table and is `Coordinator`-only. `Blocked` with a
  (future) `blocked_reason` carries more information than silent
  re-queuing would.
- Reviewer is deliberately absent from every edge. `run_reviewer`
  returns a typed `Verdict` and the `Coordinator` routes based on
  it: `Verdict::Reject` fires `InReview -> InProgress`,
  `Verdict::Approve` routes the Bundle to the `integrator` crate
  which fires `InReview -> Integrated`. Full rationale in
  [`docs/roles-and-states.md`](../../../../docs/roles-and-states.md)
  under "How Reviews Flow."

v3's 9-state table is recovered if you remove every row ending in
`Superseded` and drop the `Superseded` variant.

#### `Work` record (new, `work.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Record)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub id: WorkId,
    #[record(indexed)]
    pub parent_id: PlanId,
    pub updated_at: i64,
    pub created_at: i64,
    pub title: String,
    #[serde(default)]
    pub assignee: Option<String>,
    #[record(indexed)]
    pub status: WorkStatus,
    #[serde(default)]
    pub dependencies: Vec<WorkId>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    #[serde(default)]
    pub attempt_count: u32,
    #[serde(default)]
    pub session_failure_count: u32,
}
```

Field-by-field rationale:

| Field | Origin | Note |
|---|---|---|
| `id` | v3 + v4 | typed via `WorkId`, not `String` |
| `parent_id` | v3 + v4 | **D1**: typed `PlanId` (Stage 6 flat Plan→Work). Indexed: Stage 7's reactive coordinator iterates "child Works of this Plan" on every tick; a scan would be the v5 equivalent of v4's decomposer-loop perf cliff. Becomes `ParentId` enum when Phase lands in a later stage. |
| `updated_at`, `created_at` | v3 + v4 | Record derive requires `updated_at`; both are `i64` ms. |
| `title` | v3 + v4 | Short descriptive name. No `description` (v0.1.96 removed it). |
| `assignee` | v3 + v4 | **D4**: Stage 7 populates; Stage 6 defaults to `None`. |
| `status` | v3 + v4 | Indexed for "find all Works in state X" queries. `#[record(indexed)]` emits `to_string` into the SQLite index column. |
| `dependencies` | v3 + v4 | Typed `Vec<WorkId>`, not `Vec<String>`. The decomposer emits titles → resolved to `WorkId` server-side (scope memo D8). |
| `files` | v3 + v4 | **D4**: Stage 8 Reviewer reads to inject HEAD contents. Stage 6 defaults empty. |
| `acceptance_criteria` | v3 + v4 | Ported. Required for a Work to be eligible to run. |
| `attempt_count` | v3 + v4 | **D4**: cycling penalty. Stage 7's coordinator increments; Stage 6 defaults to 0. |
| `session_failure_count` | v3 + v4 | **D4**: consecutive crash/cancel count. Defaults to 0. |
| — | — | **Omitted:** `blocked_reason` (D3, defer). No `description` (v0.1.96 removed). |

Serde posture:
- `deny_unknown_fields` to match Stage 5's Plan; catches schema drift.
- `#[serde(default)]` on every optional-ish field so a freshly authored
  Work only needs `id`, `parent_id`, `updated_at`, `created_at`, `title`,
  `status` at minimum. This lets later stages add fields without breaking
  older JSONL.
- Indexed fields: `status` and `parent_id` (matches v3/v4). `status`
  supports "find all Works in state X"; `parent_id` supports "list
  children of Plan X," which Stage 7's reactive coordinator issues on
  every tick. Architect round 2 flagged un-indexed `parent_id` as a
  perf cliff for the coordinator loop — ship indexed from day one.

### API Design

```rust
impl Work {
    /// Create a new Work under a given Plan. Status starts `Pending`:
    /// scope memo U+1 + `project-reactive-execution-model` memory say
    /// Works are born Pending, not Draft.
    pub fn new(parent_id: PlanId, title: String) -> Self;

    /// Read the current status.
    pub fn status(&self) -> WorkStatus;

    /// Validated FSM transition. On `Transition::Changed`, mutates
    /// `self.status` and `self.updated_at`; `Unchanged` leaves state intact.
    /// Returns `FsmError<WorkStatus>` on invalid edge or role.
    pub fn transition(
        &mut self,
        target: WorkStatus,
        role: Role,
    ) -> Result<Transition, FsmError<WorkStatus>>;

    /// Validated FSM override transition — tries `validate_transition` first
    /// and falls through to the override table on rejection. Same mutation
    /// rules as `transition`.
    pub fn override_status(
        &mut self,
        target: WorkStatus,
        role: Role,
    ) -> Result<Transition, FsmError<WorkStatus>>;
}
```

No `force_status` (v3/v4 had it for bootstrap/test recovery). Not needed
for Stage 6; `override_status` covers the legitimate cases and test code
can construct a `Work` with the exact desired `status` inline via field
access.

`Work::new` chooses `Pending` as the birth state so a freshly-decomposed
Work is immediately eligible to transition to `Ready` once its deps clear —
matching v4 `work.yml` which reserves `Draft` for pre-decomposition
authoring. Stage 6's decomposer never constructs a `Draft` Work.

### Implementation Plan

"Phase" in this section refers to sub-steps inside Stage 6's first design
doc (this one). It is not the Plan/Spec/Phase/Work hierarchy level "Phase"
(which Stage 6 deliberately does not introduce). Each sub-phase is a
commit-sized chunk.

#### Phase 1: Scaffold new types
**Model:** sonnet

- Add `AcceptanceCriteria` in `crates/domain/src/criteria.rs` + re-export.
- Add `id_type!(WorkId, "wk")` in `crates/domain/src/id.rs` + re-export.
- Wire the new modules into `lib.rs`.
- Compile check: `cargo check -p domain` passes.

#### Phase 2: `WorkStatus` FSM
**Model:** sonnet

- Create `crates/domain/src/work.rs` with the `WorkStatus` enum and the
  `#[fsm(...)]` attribute exactly as specified in Data Model.
- Re-export `WorkStatus` from `lib.rs`.
- Compile check passes; `#[derive(Fsm)]` emits `validate_transition`,
  `validate_override`, `is_terminal` methods.

#### Phase 3: `Work` record + constructor + methods
**Model:** sonnet

- Add `Work` struct with the full field set, `#[derive(Record)]`,
  `Record::indexed_fields` emits `status`.
- Implement `new`, `status`, `transition`, `override_status`.
- Re-export `Work` from `lib.rs`.
- Compile check passes.

#### Phase 4: Tests
**Model:** sonnet

Two locations:

- `crates/domain/src/id/tests.rs` — extend the existing file with a
  WorkId block mirroring the PlanId block already there:
  new-prefix assertion (must start with `wk-`), Display matches AsRef,
  FromStr round-trip (Infallible), serde-transparent wire form,
  1000-iteration uniqueness sample. ~6 tests.

- `crates/domain/tests/work.rs` — new integration test file (mirrors the
  existing `tests/plan.rs`). Sections:
  - **AcceptanceCriteria:** default empty, is_empty / len, serde-
    transparent wire form `["a","b"]` not `{"0":[...]}`, non-empty
    round-trip, unicode-safe round-trip. ~5 tests.
  - **Work::new:** status defaults to **Pending** (not Draft — scope
    memo U+1 / memory `project-reactive-execution-model`), parent_id
    preserved, title preserved, id has `wk-` prefix, created_at ==
    updated_at, distinct calls produce distinct ids. ~6 tests.
  - **Work serde:** full round-trip JSON, `deny_unknown_fields` rejects
    `{"bonus": "x"}`, `#[serde(default)]` accepts minimal JSON
    (id, parent_id, updated_at, created_at, title, status only),
    status wire form is lowercase string, unknown status string errors
    cleanly. ~5 tests.
  - **Record trait:** `Record::id` returns inner str, `Record::updated_at`
    returns the field, `Work::collection_name()` is `"works"`,
    `Record::indexed_fields` contains exactly two entries keyed
    `"status"` and `"parent_id"`. The `"status"` value is the lowercase
    serde wire form (when Work status is e.g. `InProgress`, the indexed
    value is `"inprogress"`). The `"parent_id"` value is the `PlanId`'s
    `as_ref()` (e.g. `"pl-abc12"`), matching its serde-transparent wire
    form. These are the **load-bearing consistency checks** between
    Display/serde/indexed — they catch the class of bug where a rename
    on the enum or the `PlanId` prefix drifts the wire form from the
    SQLite index value. ~6 tests.
  - **FSM transition happy path:** one success test per outgoing edge in
    the transitions table. 25 edges → 25 tests. Each asserts
    `Transition::Changed`, status mutation, `updated_at` bump. This is
    exhaustive on purpose: the derive-macro's own tests (see
    `crates/derive/tests/fsm.rs`) cover the macro; this file covers the
    specific table WorkStatus commits to.
  - **FSM transition reject paths:** wrong-role rejected (e.g. Implementer
    cannot move Draft → Pending), no-edge rejected (Pending → Done
    bypass), terminal-source rejected (Done → anything). ~5 tests.
  - **FSM override:** `override_status` falls through to the override
    table when `transition` fails (`InProgress → Ready by Coordinator`
    via override); `override_status` on a valid transition edge returns
    `Transition::Changed` not `Transition::Override`; `override_status`
    with a role that's not in the override table rejects;
    `Work::transition(Done, Coordinator)` from `Ready` REJECTS
    (structural enforcement of the no-AC-skipping rule), while
    `Work::override_status(Done, Coordinator)` from `Ready` succeeds
    with `Transition::Override`. ~5 tests.
  - **FSM Unchanged:** `transition(Pending, Coordinator)` on a Work
    already in Pending returns `Transition::Unchanged` and does NOT
    bump `updated_at`. ~1 test.
  - **`is_terminal`:** parametric — `Done`, `Superseded`, `Abandoned`
    return true; every other variant returns false. ~1 test.

Total: ~57 tests across the two files. The bulk is the 25-edge FSM sweep,
which is intentional — hierarchy.md is the design doc that locks the
transitions table, and catching a typo there at test time is cheaper than
catching it in Stage 7's first agent run.

- `cargo test -p domain` passes.
- `otto ci` at `crates/domain/` passes.

#### Phase 5: Ship
**Model:** sonnet

- Update design doc status → Implemented.
- Commit (one commit for the whole stage-6 domain addition), `bump -a`,
  push, install. `hierarchy.md` is done; `llm-client.md` is the next
  design doc to write.

`WorksStore` integration in `crates/store/` is intentionally out of
scope for this doc. It lands in `plan-then-decompose.md` (the decomposer
is the first and only Stage-6 consumer of `WorksStore`, so the shim
ships alongside its call site rather than orphaned under domain).

## Alternatives Considered

### Alternative 1: Ship v3's 9-state `WorkStatus` verbatim

- **Description:** Port v3 `work.rs` as-is. No `Superseded` state. "Replace
  a Work" expressed as a new Work depending on the old (`Abandoned`) one.
- **Pros:** One less state to reason about. No cross-stage state churn for
  Stage 7 if `Superseded` turns out to be the wrong word.
- **Cons:** Breaks symmetry with `PlanStatus`, which already has
  `Superseded` as a terminal in v5. A Work whose parent goal was
  reformulated semantically fits `Superseded` better than `Abandoned`
  (the latter is "we gave up on this" rather than "a better version
  exists"). Architect flagged this in round 1 as a v3-inherited defect.
- **Why not chosen:** Scope memo D2.

### Alternative 2: `Work.parent_id: String` (opaque)

- **Description:** Keep the v3/v4 shape literally — a plain `String` that
  carries `pl-*` today and `ph-*` when Phase lands.
- **Pros:** Zero migration when Phase arrives.
- **Cons:** Throws away v5's typed-ID tooling. Invalid `parent_id` strings
  (typo, wrong prefix) pass serde validation; the first compile error
  surfaces at query time. Architect flagged this in round 1.
- **Why not chosen:** Scope memo D1 (flipped from user's initial v3
  mirror). Migration to `ParentId` enum is one field-type change + one
  `From<PlanId>` impl when earned.

### Alternative 3: FSM guards (`deps-ready`, `no-sessions`) in-doc at Stage 6

- **Description:** Extend `#[derive(Fsm)]` to accept a `guards(...)` block
  mirroring v4 `work.yml`. Implement the deps-satisfied and
  no-active-sessions checks now.
- **Pros:** Airlock the FSM at the derive layer; Stage 7's Coordinator gets
  the guarantees automatically.
- **Cons:** The guard bodies require context the `domain` crate is forbidden
  to see (deps-satisfied needs the `Store` to look up sibling Works;
  no-active-sessions needs agent-session state from `agents`/`worktree`).
  The derive would have to emit an open-ended closure or trait method,
  collapsing the clean "domain is pure symbols" boundary.
- **Why not chosen:** Enforce guards in Stage 7 method bodies (Coordinator
  checks deps, Integrator checks sessions). `domain`-layer FSM validates
  role + edge; the business rule sits at the caller. Track in Open
  Questions.

### Alternative 4: Auto-generate `Work::new(parent: &Plan, ...)` instead of `(parent_id: PlanId, ...)`

- **Description:** Take a `&Plan` reference in the constructor so mis-
  pairings of Work and Plan are unrepresentable.
- **Pros:** Even stronger type-level coupling.
- **Cons:** Forces every call site to hold a live `&Plan`; the decomposer
  has the `PlanId` more often than the full Plan record. Adds a borrow that
  buys nothing the `PlanId` typing doesn't already give.
- **Why not chosen:** Overkill.

## Technical Considerations

### Dependencies

No new crate dependencies. Uses:
- `derive` — `Fsm`, `Record`, existing.
- `taskstore-traits` (via workspace) — `Record`, `IndexValue`.
- `serde` — shared.
- `strum` — for `Display` via `#[strum(serialize_all = "lowercase")]` on
  `WorkStatus`, matching `PlanStatus`.

### Performance

Negligible. `Work` is ~100 bytes serialized; `works.jsonl` has tens to
hundreds of records per target at Stage 9 E2E scale. FSM validation is O(1)
const-table lookup.

### Security

None new. `deny_unknown_fields` catches schema-poisoning attempts on the
JSONL boundary.

### Testing Strategy

Two test surfaces inside the `domain` crate:

1. **Source-module tests** (`crates/domain/src/id/tests.rs`) — existing
   file, extended with a WorkId smoke block alongside the PlanId block.
   Covers the `id_type!` macro output: prefix, Display/AsRef/FromStr
   round-trip, serde transparency, uniqueness sample.

2. **Integration tests** (`crates/domain/tests/work.rs`) — new file, black-
   box over the domain crate's public API. Covers `AcceptanceCriteria`,
   `Work::new`, `Work` serde, `Record` trait, FSM transitions (25-edge
   sweep), FSM overrides, FSM `Unchanged`, `is_terminal`. See Phase 4 for
   the full breakdown (~55 tests total).

Exhaustiveness target for the FSM sweep: every `from → by role` edge in
the transitions table has a dedicated success test; every terminal source
has a rejection test; the override table has both fall-through-from-
transition and role-rejection tests. v3's `src/tests/fsm/work.rs` and
v4's `src/tests/fsm/work.rs` are useful references for intent but not
verbatim lifts — v5's macro-derived API exposes `WorkStatus::
validate_transition(from, to, role)` directly rather than via a runtime
interpreter.

Store-seam tests for the eventual `WorksStore<'a>` shim are explicitly
out of scope here; they land in `plan-then-decompose.md` alongside the
shim they exercise.

### Rollout Plan

- Strictly single-crate change: `domain` gains types; downstream crates
  compile unchanged because nothing has imported `domain::Work` yet.
- No migration needed for existing `.loopr/taskstore/` directories; the
  new `works.jsonl` file is created lazily by the (future) `WorksStore`
  on its first write.
- The eventual `WorksStore<'a>` shim in `crates/store/` is the only
  place in the workspace that would need to reference `Work` before
  `plan-then-decompose.md` lands. Shipping this doc alone leaves
  everything compiling without wiring; the domain crate exports the
  types, nobody imports them yet.
- One commit, one version bump (patch).

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| FSM transition table has a typo that the macro doesn't catch (wrong role, wrong target variant name). | Low | Medium | Derive-macro's own invariants (see §Data Model) + exhaustive 25-edge sweep in Phase 4. |
| `#[record(indexed)]` on `status` yields wrong index value because `WorkStatus::to_string` lowercases but the Record derive doesn't (or vice versa). | Low | Medium | Matches `Plan` exactly (`#[strum(serialize_all = "lowercase")]` + Record derive emits `to_string`). Phase 4 "load-bearing consistency check" test asserts indexed value literal matches serde wire form for every variant. |
| `parent_id: PlanId` locks Stage 6 into flat-only and makes Phase integration hard later. | Medium | Low | Scope memo D1 explicitly tracks the `ParentId` enum as the migration. Stage 7 writes the enum; `From<PlanId> for ParentId` covers existing JSONL. |
| Guards (deps-ready, no-sessions) silently don't fire because we chose to enforce them in method bodies. | Medium | Medium | Stage 7's Coordinator implements them as pre-checks before calling `Work::transition`. Integration tests in Stage 7 cover the deps-wait and session-wait paths. Flag in Open Questions. |
| `blocked_reason` omission turns out to be load-bearing and we have to migrate. | Low | Low | Additive serde field with `#[serde(default)] pub blocked_reason: Option<BlockedReason>` — zero on-disk migration. |
| `id_type!(WorkId, "wk")` prefix typo (e.g. `"wo"`) passes compilation but pollutes JSONL with ill-formed ids. | Low | High | Phase 4's `new_has_prefix` test asserts `WorkId::new().as_ref().starts_with("wk-")`. Macro's output is deterministic. |
| `WorkId::new()` collision silently overwrites via `taskstore_async::AsyncStore::create_many`. | Very Low | Medium | 5-char base36 → 60M entropy; 50% birthday collision at ~7750 records; Stage 9 runs produce fewer than 100 Works per repo. If `create_many` is `INSERT OR REPLACE` semantics, a pre-check like `PlansStore::create` does would restore the explicit `AlreadyExists` path. Deferred to `plan-then-decompose.md` (the `WorksStore` owner); here we only note the risk. |
| `parent_id: PlanId` referential integrity — a `Work` could point at a nonexistent `PlanId` if JSONL is hand-edited or a Plan is force-deleted. | Low | Low | v3/v4 have the same property: records reference by id, the Store trusts them. Stage 9's E2E never exercises the hand-edit path; a cleanup utility can be added later. |
| The `deny_unknown_fields` posture blocks forward compatibility (a Stage-6 build reading a Stage-7-written JSONL with a new field will reject). | Low | Low | v5 runs one version per target; downgrade is not supported. Cross-version reads were never a design goal. Document the stance here. |

## Open Questions

- [ ] FSM guards — confirm Stage 7 design doc commits to enforcing
      `deps-ready` and `no-sessions-on-done` in method bodies, not in the
      macro.
- [ ] `Plan.decomposition_attempts` / `Plan.bubble_up_count` — do these
      arrive with Stage 7's reactive coordinator, or are they a Stage 6
      `plan-then-decompose.md` concern (decomposer increments on failure)?
- [ ] Cross-file atomicity for plans.jsonl ↔ works.jsonl during
      decomposition — confirmed Stage 7's crash-recovery scope, but the
      companion `plan-then-decompose.md` should document the failure
      modes it expects Stage 7 to handle.
- [ ] `InProgress → Blocked` role set — ships as `(Coordinator,
      Implementer)` in this doc. If Stage 7's agents-crate design reveals
      the Director also needs this edge (e.g. manual intervention that
      freezes a Work without superseding), widen then.
- [ ] Birth-state alternate constructor — `Work::new` defaults to
      `Pending` (decomposer's birth case). If Stage 7+ introduces user-
      authored Work templates (pre-decomposition drafting), add
      `Work::draft(parent, title)` that births `Draft`; the FSM already
      models `Draft → Ready` / `Draft → Pending` so no table change is
      needed.
- [ ] Should the typed `Vec<WorkId>` dependencies be an **indexed** field
      too? Queries like "which Works block this one" become cheap with
      an index but taskstore's indexed_fields emit `IndexValue::String`
      — a vector doesn't fit that shape. Likely a Stage 7 concern when
      deps-driven reactor needs the lookup; document it here so we don't
      stumble over the IndexValue shape later.

## References

- [Scope memo](../../../../docs/design/2026-04-20-stage-6-scope.md) — decisions matrix.
- [`crates/domain/docs/design/2026-04-20-records.md`](./2026-04-20-records.md) — `Plan` record
  design; this doc mirrors structure and test strategy.
- [`docs/design/2026-04-20-fsm-macro.md`](../../../../docs/design/2026-04-20-fsm-macro.md) —
  `#[derive(Fsm)]` contract.
- [`crates/derive/docs/design/2026-04-20-record-macro.md`](../../../derive/docs/design/2026-04-20-record-macro.md)
  — `#[derive(Record)]` contract.
- `~/repos/scottidler/loopr/src/domain/work.rs` — v3's `Work`, the
  structural baseline.
- `~/repos/scottidler/loopr-v4/resources/engine/fsm/work.yml` — v4's FSM
  table, the source for the 10-state shape this doc ports.
- [`docs/vision.md`](../../../../docs/vision.md) — architectural shape, target-repo layout,
  Record/Store separation of concerns.
- [`docs/roles-and-states.md`](../../../../docs/roles-and-states.md) — canonical reference
  for the seven `Role` variants and the FSMs that reference them;
  source for the Reviewer-has-no-FSM-edges decision and the
  Coordinator-vs-Director split called out in the "Notable edges"
  note above.
