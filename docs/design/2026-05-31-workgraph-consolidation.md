# Design Document: WorkGraph consolidation

**Author:** Scott Idler
**Date:** 2026-05-31
**Status:** Implemented
**Crates touched:** domain, decomposer, loopr
**Review Passes Completed:** 5/5

## Summary

loopr-v5 hand-rolls topological-DAG logic in four places that each re-derive the
child-dependency graph by scanning sibling `Work` records (or, at decompose-time,
title strings). This doc introduces a single `WorkGraph` type in `crates/domain`
that owns the edges and answers the two queries the runtime needs (`ready_set`,
`dependents_of`) plus cycle rejection, retiring the four hand-rolled sites. It is a
consolidation, not a behavior change: `ready_set` returns the *full* set of ready
Works and each dispatch site spawns *all* of them, exactly as today. The graphs are
tiny (`n <= 5` children per decompose).

## Problem Statement

### Background

Four sites independently reason about the same child-dependency DAG:

1. **Dep-gate partition** - `crates/loopr/src/transport/handler.rs:229`. After
   decompose + persist, partitions Works into "unblocked" (`all_deps_done`) and
   "held" so only ready Works get an Implementer spawned immediately.
2. **`promote_unblocked_siblings`** - `crates/loopr/src/daemon/context.rs:996`. On a
   Work reaching `Done`, lists siblings, filters `Pending`, and promotes those whose
   deps are now all `Done` (`all_deps_done`).
3. **`block_dependent_siblings`** - `crates/loopr/src/daemon/context.rs:1037`. On a
   Work reaching an irrecoverable terminal state (`Abandoned`/`Superseded`), lists
   siblings and blocks `Pending` Works that depend on the terminal one.
4. **`detect_cycles` (+ `resolve_deps`, `normalize`)** - `crates/decomposer/src/cycles.rs`.
   Build-time Kahn's-algorithm cycle detection over the title-keyed dependency map,
   plus title-to-`WorkId` resolution.

Sites 1-3 share the `Work::all_deps_done(&[Work])` / `Work::any_dep_irrecoverable`
primitives in `crates/domain/src/work.rs`; site 4 is a separate title-string graph
in the decomposer. There is no single owner of "the edges."

### Problem

The DAG topology and its queries are derived four times in three crates. Each site
re-scans the full sibling slice and re-expresses the adjacency inline. There is no
named type for "the dependency graph of a Plan's Works," so:

- The cycle-detection algorithm (Kahn's) and the runtime ready/blocked queries live
  apart and can drift.
- The reverse-edge query (`dependents_of`) is open-coded as a filter predicate.
- A future reader must read three crates to understand how Work dependencies gate
  dispatch.

### Goals

- One typed owner of the child-dependency edges: `WorkGraph`, in `crates/domain`.
- Two queries replacing the open-coded scans: `ready_set`, `dependents_of`.
- Cycle rejection folded into construction, retiring `detect_cycles`.
- No behavior change at the four sites: identical dispatch/block/cycle decisions.
- A no-coexistence migration: the duplicated batch-scan logic and `detect_cycles` are
  removed, not dual-pathed. (Per-node `Work` primitives with non-scan callers -
  `all_deps_done`, `any_dep_irrecoverable` - are retained; see Phase 3.)

### Non-Goals

- **Bounding concurrency (a `jobs` cap).** Today every ready Work is dispatched (an
  implementer task spawned per ready Work at `handler.rs:254-258` and
  `promote:1016-1022`); real concurrency is bounded downstream by worktree allocation
  and the lane semaphores in `tools`, not by dispatch. This doc preserves that exactly.
  `ready_set` returns the full `Vec<WorkId>` - that shape *is* the on-ramp - but no
  slot-capped dispatch and no `max-parallel-works` config knob is added here. (Note:
  vision.md:626's "one Work at a time" sits in the stale "Not in First Gate" list - the
  Director beside it has since shipped - and current dispatch already exceeds it;
  formalizing a concurrency cap is the future feature, tracked at roadmap.md:178.)
- **A new graph crate or third-party graph library.** Rejected below; the type is one
  file in `domain`, no `petgraph`/`daggy`.
- **Caching the graph across calls.** Rebuilt on demand from the freshly-listed
  siblings, matching today's re-scan. (Alternatives Considered.)
- **Touching `resolve_deps` / `normalize`.** Title-to-id resolution is decomposer-
  specific and stays in the decomposer; only the cycle *algorithm* moves.

## Proposed Solution

### Overview

Add `crates/domain/src/graph.rs` with a `WorkGraph` type that holds the topology
(forward dependency edges + reverse dependent edges) and rejects cycles at
construction. Status is **not** stored on the graph - it is supplied at query time by
the caller from the same sibling slice it already lists, keeping the graph a pure
topology object consistent with the stateless-rebuild decision.

Two constructors cover the two key spaces, and they differ in fallibility on purpose:
- `from_works(&[Work]) -> Self` - runtime, keyed by `WorkId` via each Work's
  `dependencies`. **Infallible:** cycle-freedom is a decompose-time invariant
  (decomposer's CLAUDE.md: well-formedness checks belong at produce-time), and
  `Work.dependencies` is never mutated after decompose, so runtime construction trusts
  the invariant and never re-validates. Even if that premise were violated, the failure
  mode is benign: `ready_set` simply never marks a cyclic node ready (it degrades to a
  Pending hang, not a panic), so the infallible choice is safe either way.
- `from_edges(...) -> Result<Self, GraphError>` - decompose-time, built after
  `resolve_deps` maps titles to the already-pre-minted `WorkId`s. **Fallible:** this is
  where cycles are rejected, once, before the Works are persisted.

### Architecture

```
                 crates/domain
                 +--------------------------+
                 |  graph.rs                |
                 |    struct WorkGraph      |
                 |      from_works(&[Work]) |
                 |      from_edges(...)     |
                 |      ready_set(done)     |
                 |      dependents_of(id)   |
                 |    enum GraphError       |
                 +-----------+--------------+
                             ^ depends on domain (existing edge)
            +----------------+----------------+
            |                                 |
   crates/decomposer                   crates/loopr
   decompose.rs:                       handler.rs (dep-gate)
     from_edges -> cycle check         context.rs (promote / block)
     (retires cycles.rs::detect_cycles)
```

No new crate edges: `decomposer` and `loopr` already depend on `domain`.

### Data Model

```rust
// crates/domain/src/graph.rs

use std::collections::{HashMap, HashSet};
use crate::{Work, WorkId};

/// The child-dependency DAG of a single Plan's Works. Pure topology:
/// forward edges (a Work -> the Works it depends on) and reverse edges
/// (a Work -> the Works that depend on it). Status is supplied at query
/// time, not stored, so the graph can be rebuilt cheaply from a freshly
/// listed sibling slice.
pub struct WorkGraph {
    /// node -> its dependency ids (forward edges).
    deps: HashMap<WorkId, Vec<WorkId>>,
    /// node -> ids that depend on it (reverse edges).
    dependents: HashMap<WorkId, Vec<WorkId>>,
}

/// Hand-rolled `Display` + `std::error::Error` to match `domain`'s
/// existing `FsmError` style (`crates/domain/src/fsm.rs`); `domain` has
/// no `thiserror` dependency and this design does not add one.
#[derive(Debug)]
pub enum GraphError {
    /// The dependency edges form a cycle. Carries the typed node ids
    /// participating in the cycle (Kahn's leftover set) so callers can
    /// map them programmatically (e.g. id -> title) without re-parsing a
    /// string. `Display` renders the comma-joined ids for log/wire use.
    Cycle(Vec<WorkId>),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::Cycle(ids) => {
                let joined = ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(", ");
                write!(f, "dependency cycle among: {joined}")
            }
        }
    }
}

impl std::error::Error for GraphError {}
```

### API Design

```rust
impl WorkGraph {
    /// Build from persisted Works via each Work's `dependencies` field.
    /// Infallible (see Overview): does not re-check cycles. Dep ids
    /// referencing Works absent from `works` are retained as edges (the
    /// caller already warns on unknown dep ids at the dep-gate); they
    /// simply never become satisfiable because they can't appear in `done`.
    pub fn from_works(works: &[Work]) -> Self;

    /// Build from explicit (node, deps) edges, for decompose-time use
    /// where Works are not yet persisted but WorkIds are pre-minted.
    /// Rejects cycles via the ported Kahn's algorithm.
    pub fn from_edges(
        edges: impl IntoIterator<Item = (WorkId, Vec<WorkId>)>,
    ) -> Result<Self, GraphError>;

    /// Every node whose dependency ids are all in `done` and which is
    /// not itself in `done`. A no-dependency node is ready (empty subset
    /// of `done`); a node already in `done` is finished, not ready, so it
    /// is excluded. Returns nodes regardless of their non-done status;
    /// callers intersect with the status they care about (e.g. `Pending`).
    /// Excluding `done` keeps the returned Vec to live candidates rather
    /// than re-returning historical works the caller would filter away.
    pub fn ready_set(&self, done: &HashSet<WorkId>) -> Vec<WorkId>;

    /// Direct reverse edges: the nodes that list `node` in their deps.
    /// Returns `&[]` for a node with no dependents. Note: a node absent
    /// from the constructing `&[Work]` but named as a dependency by a
    /// present node still has reverse edges tracked, so `dependents_of`
    /// may return dependents for such a "phantom" node.
    pub fn dependents_of(&self, node: &WorkId) -> &[WorkId];
}
```

`from_edges`'s cycle detection is the Kahn's algorithm ported from
`cycles.rs::detect_cycles`, re-keyed from `&str` titles to `WorkId`. The `node_count`
span field and `err` attribute carry over. The leftover (nonzero in-degree) set is
returned as `GraphError::Cycle(Vec<WorkId>)` - typed ids, not a pre-joined string - so
the decompose site maps those ids back to titles programmatically for the operator-
facing message (see Site 4).

### Call-site rewrites (behavior-preserving)

**Site 1 - dep-gate** (`handler.rs:229`). Build `from_works(&works)`; compute
`done = {w.id : w.status == Done}` (empty on a fresh decompose); `unblocked =
ready_set(&done)`, `held = works \ unblocked`. The unknown-dep-id warning loop is
unchanged. Net: identical partition; the `all_deps_done` predicate becomes a set query.

**Site 2 - `promote_unblocked_siblings`** (`context.rs:1010-1022`). Build
`from_works(&siblings)`; `done = {Done ids}`; promote `{Pending} ∩ ready_set(done)`.

**Site 3 - `block_dependent_siblings`** (`context.rs:1058-1062`). Build
`from_works(&siblings)`; iterate `dependents_of(&terminal_work_id)`, filter the
`Pending` ones, block them. Identical to today's inline `dependencies.contains` filter.

**Site 4 - decompose** (`decompose.rs:271-377`). Reorder so resolution precedes the
cycle check: `normalize` -> pre-mint `title_to_id` -> `resolve_deps` (title->id) ->
`WorkGraph::from_edges(child_ids.zip(resolved_deps))`. On `Err(GraphError::Cycle(ids))`
- where `ids: Vec<WorkId>` - **map each id back to its title** via an inverted
`title_to_id` (`HashMap<WorkId, String>`) so the `DecomposerError::CycleDetected`
message stays human-readable (the old `detect_cycles` named titles; an id-keyed graph
would otherwise name opaque `WorkId`s - a UX regression). The typed-`Vec<WorkId>`
payload means this is a direct map lookup, never string parsing. The inversion is a
safe bijection because duplicate normalized titles are already rejected upstream
(`decompose.rs:273`); a one-line comment should record that dependency. Deletes
`detect_cycles` and the title-keyed `dep_graph` construction; `resolve_deps` and
`normalize` survive in the renamed `resolve.rs` (see Phase 2).

The three runtime sites call the infallible `from_works`, so none of them gains an
error path - the rewrite is a like-for-like substitution of the inline scan, not a new
fallible call.

### Implementation Plan

#### Phase 1: `WorkGraph` type + cycle port
**Model:** opus
- Add `crates/domain/src/graph.rs`: struct, `GraphError::Cycle(Vec<WorkId>)`
  (hand-rolled Display + Error), infallible `from_works`, fallible `from_edges`,
  `ready_set` (excludes nodes already in `done`), `dependents_of`. Port Kahn's from
  `cycles.rs::detect_cycles` into `from_edges`, re-keyed to `WorkId`, returning the
  leftover set as `Vec<WorkId>`, with the same
  `#[tracing::instrument(level="debug", fields(node_count), err)]`.
- `pub mod graph;` + re-export `WorkGraph` / `GraphError` from `domain`'s lib root.
- `crates/domain/src/graph/tests.rs` (declared `#[cfg(test)] mod tests;`): cycle
  rejection (incl. self-loop) returns the offending `Vec<WorkId>`, `ready_set` with
  empty/partial/full `done`, no-dep node always ready, node-in-`done` excluded from
  `ready_set`, `dependents_of` direct edges + phantom-node reverse edge, unknown-dep-id
  edge retained.

#### Phase 2: Rewire decomposer; retire `detect_cycles`
**Model:** opus
- Reorder `decompose.rs` to resolve-then-graph; on `GraphError::Cycle(ids)`, invert
  `title_to_id` and map ids back to titles before wrapping in
  `DecomposerError::CycleDetected` (preserve the title-named message).
- Delete `cycles.rs::detect_cycles` and its title-keyed `dep_graph` construction.
- Rename `crates/decomposer/src/cycles.rs` -> `resolve.rs`, keeping `resolve_deps` +
  `normalize`; update the `use crate::cycles::{...}` import at `decompose.rs:32` to
  `crate::resolve::{...}` and rename the test module dir `cycles/tests.rs` ->
  `resolve/tests.rs` (dropping the now-deleted cycle tests, keeping resolve/normalize).
- Update `crates/decomposer/CLAUDE.md` (the instrumentation section names `detect_cycles`
  and `cycles.rs`).

#### Phase 3: Rewire the three loopr sites
**Model:** sonnet
- Mechanical substitution at sites 1-3 per "Call-site rewrites." Preserve span fields,
  the unknown-dep warning, and the `Pending`-filter semantics.
- **Keep `Work::all_deps_done` AND `Work::any_dep_irrecoverable`** as per-node
  primitives. A pre-removal `rg all_deps_done` found a fifth caller the original audit
  missed: `crates/loopr/src/daemon/context/spawner.rs:210` (`assign_work`'s dep-gate
  re-check for a single Work). Both methods are legitimate per-node membership checks
  (one Work against its siblings), not the batch partition scans WorkGraph replaces;
  `spawner.rs:210` and `startup.rs:346` keep using them. Only the three batch-scan
  sites are rewired. This corrects the doc's earlier "remove `all_deps_done`" plan.

#### Phase 4: Seam test + cleanup
**Model:** sonnet
- Parity/seam test: on a representative multi-Work fixture (the Phase E dep-gate
  scenarios in `crates/loopr/tests/dep_gate.rs`), assert the WorkGraph path yields the
  same unblocked/held partition the old `all_deps_done` partition produced.
- `otto ci` at workspace root green; `whitespace -r`.

## Alternatives Considered

### Alternative 1: Pull in `petgraph` or `daggy`
- **Description:** Use a third-party graph crate for topology + toposort, or `daggy`
  for acyclic-by-construction.
- **Pros:** Battle-tested traversals; richer ops if the graph ever grows.
- **Cons:** A supply-chain dependency to walk a `<= 5`-node DAG; the existing Kahn's is
  ~40 working lines with tests; neither earns its keep on capability at this scale.
- **Why not chosen:** The problem is *duplication*, not a missing algorithm. One typed
  owner of the edges fixes it without a dep.

### Alternative 2: A standalone `graph` crate
- **Description:** A new workspace crate holding `WorkGraph`.
- **Pros:** Tightest blast radius for the type.
- **Cons:** It would depend on `domain` (for `Work`/`WorkId`) and host exactly one
  small, pure type - more crate ceremony than blast-radius benefit. `domain` already
  hosts the per-node primitives (`all_deps_done`, `any_dep_irrecoverable`).
- **Why not chosen:** `domain` is the natural home; every site already depends on it.

### Alternative 3: Cache the `WorkGraph` on `DaemonContext`
- **Description:** Build once, invalidate on Work status change.
- **Pros:** Avoids rebuild per event.
- **Cons:** Cache-invalidation complexity for zero measurable gain at `n <= 5`; the
  current code already re-lists siblings every event.
- **Why not chosen:** Premature. Stateless rebuild matches today's cost profile.

## Technical Considerations

### Dependencies
- No new external crates. `domain` has no `thiserror`; `GraphError` hand-rolls
  `Display` + `std::error::Error` exactly as `FsmError` (`crates/domain/src/fsm.rs`) does.
- No new crate-graph edges (`decomposer`/`loopr` already depend on `domain`).

### Performance
- `from_works` is O(E) to build both adjacency maps; `ready_set` is O(V + E);
  `dependents_of` is O(1) lookup. At `n <= 5` all are negligible.
- **Acknowledged tradeoff:** this shifts the dep-gate from `all_deps_done`'s
  zero-allocation slice scan to two `HashMap` allocations per event. That is a
  deliberate, documented cost of having one typed owner of the edges; the
  cached-`WorkGraph` alternative (Alternative 3) is the escape hatch if Plan sizes ever
  grow enough to make per-event rebuild matter. Not optimizing for that now is the
  stateless-rebuild decision, consistent with the current re-list-every-event behavior.

### Security
- None. Pure in-memory topology over already-trusted records.

### Testing Strategy
- `WorkGraph` is a transient in-memory type, not a persisted `Record`, so working-rule
  1's serde-round-trip seam test does not apply; the parity/integration test below is
  the seam coverage.
- Unit tests in `domain` (Phase 1): cycle/self-loop rejection, `ready_set` subset
  semantics, no-dep readiness, `dependents_of`, unknown-dep-id retention.
- Decomposer tests updated for the resolve-then-cycle reorder (Phase 2), including one
  asserting `CycleDetected` still names titles (not WorkIds) after the id→title remap.
- Seam/parity test in `loopr` (Phase 4): WorkGraph path == old partition on the
  existing dep-gate fixtures.

### Rollout Plan
- Single landing across `domain` + `decomposer` + `loopr` (a deliberate cross-cutting
  change per working-rule 3, which this design doc authorizes). No coexistence: the old
  `detect_cycles` and `all_deps_done` scan paths are removed in the same change.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Behavior drift at a rewired site | Med | High | Seam/parity test on existing dep-gate fixtures; sites preserve `Pending`-filter + warnings |
| Cycle error-precedence change (resolve before cycle) | Med | Low | Documented; both checks still run; resolve-first yields the clearer "unknown sibling" message first |
| Cycle error names WorkIds instead of titles | Med | Low | `GraphError::Cycle(Vec<WorkId>)` + Site 4 maps ids->titles via inverted `title_to_id`; Phase 2 test asserts it |
| `title_to_id` inversion not a bijection if dup-title rejection is relaxed | Low | Low | Safe today (`decompose.rs:273` rejects dup normalized titles); one-line comment records the dependency |
| `from_works` retains unknown dep ids as unsatisfiable edges | Low | Low | Matches current `all_deps_done` (unknown dep => false); dep-gate warning loop retained |
| A fifth caller is missed | Med | Med | MATERIALIZED: `spawner.rs:210` (`assign_work`) calls `all_deps_done` per-node; caught by pre-removal `rg`. Resolution: keep `all_deps_done` as a per-node primitive (do not remove); rewire only the three batch sites. `startup.rs:346` (`any_dep_irrecoverable`) likewise kept. |

## Open Questions

Both resolved 2026-05-31 (post Architect Design Review):

- [x] **Parallelism stays out of scope.** `ready_set` ships as the Vec-shaped on-ramp,
      but no `max-parallel-works` knob and no slot-capped dispatch are added here. Today
      dispatch already spawns all ready Works (concurrency bounded downstream by worktree
      allocation + lane semaphores); a cap is a behavior change with its own design
      surface, tracked at roadmap.md:178.
- [x] **`resolve_deps`/`normalize` move to a renamed `resolve.rs`** (was `cycles.rs`).
      Once `detect_cycles` is deleted, the `cycles.rs` name no longer describes its
      contents (pure title resolution); rename to the single-word `resolve.rs` per
      naming conventions. Imports (`decompose.rs:32`) and the test module path move with
      it.

## References
- vision.md:626 (stale "one Work at a time" first-gate note), roadmap.md:178 (parallel worktrees, deferred)
- `crates/decomposer/src/cycles.rs` (Kahn's source; renamed to `resolve.rs` after `detect_cycles` is deleted), `crates/decomposer/CLAUDE.md`
- `crates/loopr/src/transport/handler.rs:229`, `crates/loopr/src/daemon/context.rs:996-1085`
- Working-rule 3 (cross-cutting change needs a top-level design doc), `../../CLAUDE.md`
