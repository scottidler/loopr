# integrator

Deterministic, non-LLM merge-publish. Accepted Bundles into Tick.

## In scope

- `fn integrate(bundles: &[Bundle], plan: &Plan, deps: &IntegratorDeps<...>) -> Result<Tick, IntegrationError>`
- Sequential git merge of bundle branches into the integration branch (`loopr/plan-<plan-id>`)
- Conflict detection and classification (structural vs retryable) using `bundle.paths`
- Tick record creation, SHA capture, and persistence via `store::TicksStore` (including `DuplicateTick` short-circuit on crash-recovery)
- Crash-recovery idempotency: Bundles in `Integrating` are a re-entry point; `git merge-base --is-ancestor` distinguishes "merge already landed" from "merge never landed"
- Intra-daemon `git_lock` serialization of checkout/merge/rollback against the single working tree
- This crate's own `Config` struct (`IntegratorConfig`: `git_timeout`, `allow_multi_bundle`)

## Out of scope

- **LLM calls of any kind.** This crate is the non-LLM chokepoint of the pipeline. Dependencies: `domain`, `store` — emphatically NOT `llm`, NOT `agents` (which pulls `llm` transitively). The Round 1 Architect flagged this as a contradiction in the previous `runtime`-monolith design; the crate restructure makes the rule mechanically enforceable at the Cargo graph level.
- Integration-branch creation (`loopr/plan-<plan-id>`). The daemon (Stage 8 wiring capstone) creates it at Plan-start so the base SHA is deterministic; `integrate` returns `IntegrationError::IntegrationBranchMissing` if the branch is absent.
- Agent spawning or decisions about retry (`agents`, `loopr`). The driver's retry contract on `Integrating` Bundles is documented as an Invariant in `docs/design/2026-04-22-integrator.md` and must be honored by the Stage 8 wiring capstone.
- Decomposition (`decomposer`), domain records (`domain`)
- What to do when integration fails; this crate returns a typed error and the driver decides

## Earned later

- **Validation command execution.** Originally in scope; deferred per `docs/design/2026-04-22-integrator.md`. Reviewer already validates against acceptance criteria, so an integration produces a typed `Tick` as the exit criterion, not a green build. Validation will be earned via its own design doc when a real run shows a Reviewer-approved Bundle breaking the integration branch. Would reintroduce a `ToolExecutor` handle on `IntegratorDeps` and a `ValidationFailed` variant on `IntegrationError`.
- **Multi-Bundle Ticks.** `allow_multi_bundle` defaults to `false`; single-Bundle Ticks only for first gate.
- **Post-merge rollback after validation failure.** Follows validation.
- **Per-integration-worktree** for multi-Plan concurrency (Alternative 6 in the design doc).

## Rule

This crate must not depend on `llm`. Compile-time enforced via Cargo: `integrator`'s `[dependencies]` never includes `llm`. If you find yourself reaching for an LLM here, the logic belongs in `agents`; emit an event for the driver instead.

Given the same bundles and the same base commit, `integrate` produces the same Tick SHA or the same typed error. That determinism is the invariant; keep it.

## Instrumentation

`integrate` opens `integrator.integrate` at `info` with `err` and the following scope fields, inherited by every nested span:

- `plan_id`, `bundle_count`, `target` — set at span open.
- `integration_branch` — recorded after `verify_branch` resolves the actual branch name.
- `phase` — recorded as the function walks `preflight` -> `git_sequence` -> `commit`. A failing run leaves `phase` at the last point reached, so a reader can tell where the failure happened from the span alone.

Inner spans:

- `integrator.preflight_plan_consistency` (`debug`, `err`) — pre-flight per-bundle Plan check.
- `integrator.transition_bundle` (`debug`, `err`) — Bundle FSM transitions; carries `bundle_id`, `work_id`, `from`, `target_status`, `expected_updated_at` (the OCC version snapshot).
- `integrator.fail_all` / `integrator.fail_all_without_reset` (`warn`) — the rollback paths; carry `bundle_count` + `error`.
- `integrator.git.checkout`, `integrator.git.merge_no_ff`, `integrator.git.is_ancestor`, `integrator.git.reset_hard` (`debug`, `err`) — the Phase 2 git operations. The lower-level `run_git` wrapper sits at `trace` with `git_args` recorded.

Acceptance test: `tests/instrumentation.rs::integrator_smoke_spans_happy_path` drives `integrate` against a real git tempdir and asserts `integrator.integrate` carries `plan_id`, `bundle_count`, `integration_branch`, the final `phase=commit`, plus the nested transition + git spans.

**Visibility (2026-05-09 sweep).** Each phase transition inside `integrate` now emits a corresponding `info!("integrator: phase begin", phase=...)` event so a stalled integration's last visible phase is unambiguous from `events.log` alone (Phase 3 of `docs/design/2026-05-09-comprehensive-telemetry.md`). The `transition_bundle`, `fail_all`, `fail_all_without_reset`, and `git.*` helpers each emit a single `debug!` or `warn!` on their success/failure path. The contract test `tests/integrator_visibility.rs` drives a real happy-path `integrate` under the production telemetry subscriber and asserts every documented span surfaces its required fields against `events.log`. Operator grep patterns: [`docs/telemetry-grep-cookbook.md`](../../docs/telemetry-grep-cookbook.md).

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [../../docs/design/2026-04-22-integrator.md](../../docs/design/2026-04-22-integrator.md): Stage 8 Integrator design; load-bearing invariants
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
