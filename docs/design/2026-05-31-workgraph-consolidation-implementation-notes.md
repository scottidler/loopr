# Implementation Notes: WorkGraph consolidation

Running, append-only record of how the implementation interprets or
diverges from `2026-05-31-workgraph-consolidation.md`. Committed with the
final-phase commit.

## Phase 1: WorkGraph type + cycle port

### Design decisions
- Tests placed in `crates/domain/tests/graph.rs` (integration tests against
  the public API), not `crates/domain/src/graph/tests.rs` as the design doc
  literally specified. Reason: domain's established convention is one
  `tests/<module>.rs` per module (`bundle.rs`, `fsm.rs`, `plan.rs`,
  `tick.rs`, `work.rs`); there are zero `src/*/tests.rs` in the crate.
  Consistency with the crate wins. All `WorkGraph` behavior is observable
  through the public API (`from_edges` -> `Result`, `ready_set`,
  `dependents_of`), so private-field access was not needed.
- `#[derive(Debug)]` added to `WorkGraph` (`graph.rs`). Required so tests can
  call `.unwrap_err()` on `Result<WorkGraph, GraphError>` (the `Ok` type must
  be `Debug`). Harmless; the maps already hold `Debug` types.
- `detect_cycle` carries the same instrumentation contract as the retired
  `cycles::detect_cycles`: `#[tracing::instrument(level="debug", skip_all,
  fields(node_count), err)]` plus a `debug!` on the ok path.

### Deviations
- Test-file location (see Design decisions) is the only departure from the
  doc's letter; the doc's intent (unit coverage of the type) is fully met.

### Tradeoffs
- `from_works` infallible vs `from_edges` fallible: kept exactly as designed.
  Cycle detection lives only in `from_edges`; runtime `from_works` trusts the
  decompose-time invariant. The shared `build` helper does the topology in
  one pass for both.

### Open questions
- None.

## Phase 2: Rewire decomposer; rename cycles.rs -> resolve.rs

### Design decisions
- Cycle-detection tests were not duplicated into `resolve/tests.rs`; they
  moved to `crates/domain/tests/graph.rs` (re-keyed to `WorkId`), which is the
  home of the algorithm now. `resolve/tests.rs` keeps only `normalize` +
  `resolve_deps` coverage.
- `decompose.rs:32` import changed `crate::cycles::{detect_cycles, normalize,
  resolve_deps}` -> `crate::resolve::{normalize, resolve_deps}`.
- `git mv` used for both `cycles.rs -> resolve.rs` and the `cycles/ ->
  resolve/` submodule dir, preserving history.

### Deviations
- The decomposer instrumentation test (`tests/instrumentation.rs`) no longer
  asserts a `detect_cycles` span; the cycle span (`detect_cycle`) is now a
  `domain` concern. The doc said "update the test"; removing the over-assert
  is that update.

### Tradeoffs
- Error precedence: `resolve_deps` now runs BEFORE cycle detection (was after).
  A response with both an unknown-sibling ref and a cycle now reports
  `UnresolvedDeps` first instead of `CycleDetected`. This is the documented
  precedence change (design doc risk table) and yields the clearer message;
  it also guarantees `WorkGraph::from_edges` sees only real edges.
- Cycle message stays title-named: the `Vec<WorkId>` payload is mapped back
  through an inverted `title_to_id` (safe bijection per `decompose.rs:273`
  dup-title rejection), so `DecomposerError::CycleDetected` reads the same as
  before to operators.

### Open questions
- None.

## Phase 3: Rewire the three loopr batch-scan sites

### Design decisions
- Site 1 (`handler.rs` dep-gate): `WorkGraph::from_works(&works)` + `ready_set`
  over a `done` set (empty on fresh decompose) reproduces the old
  `all_deps_done` partition exactly. Used fully-qualified `domain::` refs to
  match the file's style.
- Site 2 (`promote_unblocked_siblings`): `ready_set(done)` intersected with
  `Pending`. Since `ready_set` excludes `done` nodes and Pending works are
  never in `done`, the set equals the old `all_deps_done`-filtered Pending set.
- Site 3 (`block_dependent_siblings`): `dependents_of(terminal_work_id)`
  intersected with `Pending` - identical to the old inline
  `dependencies.contains(terminal_work_id)` filter.

### Correctness: ready_set-excludes-done is inert under every caller
The `ready_set` tightening (exclude nodes already in `done`) cannot regress
any site, by construction:
- Site 1 (handler): input is freshly-created works, all Pending, so `done` is
  empty; the exclusion clause can never fire.
- Site 2 (promote): the result is filtered to `Pending` after `ready_set`. A
  Pending work is never in `done`, so the `node not-in done` clause can only
  drop non-Pending nodes - which the Pending filter drops anyway. Hence
  `ready_set ∩ Pending` is exactly `all_deps_done ∩ Pending`.
- Site 3 (block): does not use `ready_set`.
So the exclusion is purely a contract refinement of `WorkGraph`, not a
behavior change at any caller.

### Deviations
- **`Work::all_deps_done` is KEPT, not removed** (design doc said remove it).
  A pre-removal `rg all_deps_done` found a fifth caller the audit missed:
  `crates/loopr/src/daemon/context/spawner.rs:210` (`assign_work`'s per-Work
  dep-gate re-check). `all_deps_done` and `any_dep_irrecoverable` are retained
  as per-node primitives; `spawner.rs:210` and `startup.rs:346` keep using
  them. Design doc + risk table updated to match. Consequently the
  `domain/tests/work.rs` and `loopr/tests/dep_gate.rs` `all_deps_done` tests
  still compile (dep_gate.rs is converted to the WorkGraph path in Phase 4).

### Tradeoffs
- Site 3 builds a full `WorkGraph` for what is a single-node reverse lookup.
  Chosen for consolidation consistency (one abstraction across all three batch
  sites); the allocation is negligible vs the per-event `list_by_parent_id`
  already on that path.

### Open questions
- None.

## Phase 4: Seam/parity test + cleanup

### Design decisions
- `crates/loopr/tests/dep_gate.rs`: the four partition tests were converted
  from the `all_deps_done` predicate to a `workgraph_partition` helper that
  reproduces handler.rs's `WorkGraph::from_works` + `ready_set(done)` path.
  They now exercise the production path (a real seam/parity test) rather than
  re-deriving it.
- Final gate: workspace-root `otto ci` green (all crates lint + check + test).

### Deviations
- `partition_done_dep_unblocks_dependent` assertion changed. Old: a Done work
  with no deps landed in `unblocked` (count 2). New: the Done work is excluded
  from `unblocked` (it is finished, not "ready") and lands in `held`; only its
  dependent B is unblocked. This follows from the `ready_set`-excludes-`done`
  tightening (Architect-agreed) and is strictly more correct - the handler must
  never spawn an implementer for an already-Done Work. It never triggers in
  production: handler partitions only freshly-created (all-Pending) works.

### Tradeoffs
- None.

### Open questions
- None.
