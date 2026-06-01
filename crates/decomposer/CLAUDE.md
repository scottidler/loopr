# decomposer

Goal into validated Plan into Spec/Phase/Work DAG. The middle-end of the pipeline.

## In scope

- `fn plan(goal: &Goal, ctx: &mut Context) -> Result<Plan>`: user intent to validated Plan
- `fn decompose(plan: &Plan, ctx: &mut Context) -> Result<WorkDag>`: Plan to typed Work DAG
- `DecomposeStrategy` trait and impls (e.g. `BriefDecompose`, `FullDecompose`), selected by config name
- Dependency resolution and cycle detection on the produced DAG
- Decomposition-specific prompts under `src/prompts/`
- This crate's own `Config` struct

## Out of scope

- Agent execution (`agents`): decomposer produces the plan-of-work and stops
- Bundle review, integration (`agents`, `integrator`)
- LLM transport (`llm`), tool trait + impls (`tools`), worktree lifecycle (`worktree`)
- Record persistence (`store`), record definitions (`domain`)
- Whether to run a decomposer at all (that's the driver's call in `loopr`)

## Rule

Output of this crate is a `WorkDag` that downstream stages can consume without re-validating its structure. If execution-time code is checking "is this DAG well-formed?", the check belongs here, at produce-time.

## Instrumentation

`decompose` is the golden `#[instrument]` example for the workspace: opens at `info` with `plan_id`, `goal_len`, `child_count` (Empty, recorded post-validation), `outcome` (Empty, recorded as `workspace_scan_failed` / `llm_failed` / `ok` / `cycle_detected` / etc. at the relevant return point). Helper functions all carry `debug` spans:

- `try_llm_once` (debug+err) — `system_chars`, `user_chars`.
- `collect_workspace_tree` — `target`, post-record `tree_chars`.
- `assemble_system` — `tree_chars`.
- `assemble_user` — `goal_len`, `retry`.
- `resolve_deps` — `child_count`, `title_count`.

Cycle detection moved out of this crate to `domain::WorkGraph::from_edges` (re-keyed from titles to `WorkId`); the former `cycles.rs` is now `resolve.rs`, holding only `resolve_deps` + `normalize`. The `node_count`/`err` span lives on `WorkGraph::detect_cycle` in `domain`. See `docs/design/2026-05-31-workgraph-consolidation.md`.

Acceptance test: `tests/instrumentation.rs::decomposer_smoke_spans_decompose` drives one happy-path decomposition and asserts the outer span carries plan_id/goal_len/child_count/outcome and the helpers all fired.

**Visibility (2026-05-09 sweep).** Inner helpers (`collect_workspace_tree`, `resolve_deps`, `assemble_system`, `assemble_user`, `try_llm_once`) each emit a `debug!` on their success path so the span ancestry lands on a real `events.log` line (Phase 4 of `docs/design/2026-05-09-comprehensive-telemetry.md`). Operator grep patterns: [`docs/telemetry-grep-cookbook.md`](../../docs/telemetry-grep-cookbook.md).

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
