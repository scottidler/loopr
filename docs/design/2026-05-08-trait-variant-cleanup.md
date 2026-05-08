# Design Document: trait_variant cleanup of async-fn-in-trait desugaring

**Author:** Scott Idler
**Date:** 2026-05-08
**Status:** Draft
**Crates touched:** store, integrator, agents, llm, loopr
**Review Passes Completed:** 5/5

## Summary

Replace the hand-rolled `fn method<'a>(&'a self, ...) -> impl Future<Output = ...> + Send + 'a` desugaring with `#[trait_variant::make(Send)]` plus plain `async fn`. The pattern appears at 52 sites across 20 files in 6 crates (`store`, `integrator`, `agents`, `llm`, `loopr`, plus their test files). Affects 8 traits and their forwarding/decorator/fake impls. Drops roughly 50 of the 133 lifetime annotations in the workspace and makes the trait declarations read like normal async code, with no runtime cost and no API change for callers.

## Problem Statement

### Background

When `async_trait` was removed (project memory: "Async refactor - COMPLETE; async_trait removed; validator converted") the codebase swapped the macro's `Box<dyn Future>` heap allocations for the manual desugaring stable Rust requires when an async-trait method must return a `Send` future:

```rust
pub trait BundleUpdateSink: Send + Sync {
    fn update<'a>(
        &'a self,
        bundle: Bundle,
        expected_updated_at: i64,
    ) -> impl Future<Output = Result<(), BundleUpdateError>> + Send + 'a;
}
```

This is the canonical workaround on stable. Bare `async fn` in traits compiles, but the auto-trait inference for the returned future is conservative and `Send` is not propagated; spawning the future on multi-threaded tokio breaks. Adding `+ Send` to a return-position-impl-Trait-in-trait method is not yet stable on its own (the `return_type_notation` syntax is nightly-only), so the `<'a>(&'a self, ...) -> impl Future<...> + Send + 'a` shape is the price of crossing tokio's worker boundary on stable.

The cost is paid per-trait and per-impl. A workspace grep counts 52 occurrences of `#[allow(clippy::manual_async_fn)]` across 20 files: 8 traits (`BundleUpdateSink`, `WorkUpdateSink`, `PlanUpdateSink`, `WorkLookup`, `TickSink`, `BundleSink`, `LlmClient`, `ToolExecutor`), each with 2-3 forwarding impls (`Store` / `&T` / `Arc<T>`), the `SummaryFanout<S>` decorator's three sink impls in `loopr`, the `MeteredLlmClient<L>` decorator's two `LlmClient` impls in `llm`, and per-test fakes scattered across `agents/tests/`, `agents/src/*/tests.rs`, `integrator/src/tests.rs`, and `store/src/*/tests.rs`. Each `#[allow(clippy::manual_async_fn)]` exists to silence a lint that correctly identifies the pattern as a manual desugaring; the silencing is itself a smell.

### Problem

The trait surface and every impl carry the same mechanical `<'a>(&'a self, ...) -> impl Future<...> + Send + 'a` ritual. New consumers (the validation work in `2026-05-08-validation.md` adds another `ToolExecutor` injection point that will follow the same pattern) inherit the boilerplate. Reading the trait definition no longer tells you the intent; you have to mentally re-sugar it back to `async fn`.

`trait_variant::make` is the official Rust project answer to this exact problem. It generates the desugared form from a clean `async fn` source. Adopting it is a mechanical, non-architectural change with zero runtime cost.

### Goals

- Replace the manual desugaring on every trait that requires `Send` futures with `trait_variant::make`.
- Drop every `#[allow(clippy::manual_async_fn)]` that exists solely to silence the desugaring.
- Keep the `Send` guarantee on every consumer (the daemon, the integrator, the agents) - no consumer must lose `Send` propagation.
- Keep the API surface unchanged: the public method signatures `update(&self, bundle, expected_updated_at).await` continue to work for every existing caller.

### Non-Goals

- Restructuring `Deps<...>` or any DI shape. The traits change; the dependency wiring does not.
- Eliminating Bucket 1 lifetimes (`BundlesStore<'a>` etc.). Those are structural borrows by design; this doc does not touch them.
- Eliminating Bucket 2 lifetimes (`Handlebars<'static>`, `for<'a> LookupSpan<'a>`, `Cow<'static, str>`). Those are forced by external crate APIs; this doc does not touch them.
- Migrating to `return_type_notation` (RTN). RTN is nightly-only and would couple the codebase to a nightly compiler; `trait_variant` works on stable today.
- Bringing back `async_trait`. The previous async refactor explicitly removed it to avoid `Box<dyn Future>` allocations; this doc continues that direction.

## Proposed Solution

### Overview

Add `trait-variant` to the workspace dependency table. For each affected trait, replace the manual desugaring with the macro's in-place form:

```rust
#[trait_variant::make(Send)]
pub trait BundleUpdateSink {
    async fn update(&self, bundle: Bundle, expected_updated_at: i64) -> Result<(), BundleUpdateError>;
}
```

The macro rewrites the trait in place: `async fn update(&self, ...) -> Result<...>` expands to the `<'a>(&'a self, ...) -> impl Future<Output = ...> + Send + 'a` shape the codebase has hand-written today, with the `Send` bound on the returned future. The trait keeps its public name; no sibling `Local*` trait is introduced. Existing impls for `store::Store`, `&B`, and `Arc<B>` rewrite to plain `async fn`. The forwarding impls collapse from a four-line desugaring to a one-line `async fn`.

(`trait-variant` also offers a sibling-trait form, `#[trait_variant::make(NewName: Send)]` over a `LocalName` trait, which generates two traits and a blanket impl. We deliberately choose the in-place form: every consumer in the workspace requires `Send`, so a non-`Send` variant has no consumer.)

Callers (`bundles.update(b, exp).await`, `(*self).update(b, exp).await`) do not change.

### Architecture

The change is purely surface-level on the affected traits. No crate boundary moves, no Cargo dep graph shifts, no records change shape. The Cargo graph gains one workspace dep (`trait-variant`); each affected crate gains one transitive dep on it via the trait macro expansion.

`trait-variant` is a proc-macro crate; the expansion happens at compile time. The emitted code is the same `<'a>(&'a self, ...) -> impl Future<...> + Send + 'a` shape the codebase has hand-written today; the in-place form keeps the trait's public name unchanged. No runtime cost, no allocation, no virtual dispatch.

### Data Model

No record types change. `domain` is untouched. The `*UpdateSink` traits and their error types (`BundleUpdateError`, `WorkUpdateError`, `PlanUpdateError`, `BundleSinkError`) are unchanged in shape. Only the syntactic form of the method declaration changes.

### API Design

Eight traits convert. For each, the trait declaration goes from manual desugaring to `#[trait_variant::make(Send)]` + `async fn`. The trait keeps its public name; no sibling type is generated.

The existing `: Send + Sync` supertrait bound stays on the trait declaration. `trait-variant`'s in-place rewrite operates on the trait body's method signatures, not the supertrait list, so `pub trait BundleUpdateSink: Send + Sync` continues to compile.

Affected traits, their files, and the impls (real + forwarding/decorator/fakes):

| Trait | Crate | Trait def file | Impl sites |
|---|---|---|---|
| `BundleUpdateSink` | store | `src/bundles.rs` | `Store`, `&B`, `Arc<B>`; `SummaryFanout<S>` (loopr); `CollectingSink` (agents/reviewer tests) |
| `WorkUpdateSink` | store | `src/works.rs` | `Store`, `&W`, `Arc<W>`; `SummaryFanout<S>` (loopr) |
| `PlanUpdateSink` | store | `src/plans.rs` | `Store`, `&P`, `Arc<P>`; `SummaryFanout<S>` (loopr) |
| `WorkLookup` | integrator | `src/lib.rs` | `Store`, `&T`; `FakeWorks` (integrator tests) |
| `TickSink` | integrator | `src/lib.rs` | `Store`, `&T`; `FakeTicks`, `FakeBundleSink` (integrator tests) |
| `BundleSink` | agents | `src/implementer.rs` | `Store`, `&B`, `Arc<B>`; `CollectingSink` (agents/implementer tests) |
| `LlmClient` | llm | `src/client.rs` | `AnthropicClient` (llm), `MeteredLlmClient<L>` (llm), `StubLlm` (llm) |
| `ToolExecutor` | agents | `src/dispatch.rs` | `LaneRouter`-based real impl + a test impl in `dispatch/tests.rs` |

`SummaryFanout<S>` in `crates/loopr/src/daemon/summary_fanout.rs` impls all three store sink traits (`WorkUpdateSink`, `BundleUpdateSink`, `PlanUpdateSink`) with the same desugaring; those three impls collapse to `async fn`.

`MeteredLlmClient<L>` in `crates/llm/src/metered.rs` is a `LlmClient` decorator that wraps another `LlmClient`; both its trait impl methods convert to `async fn`.

Test fakes that impl these traits manually live in:

- `crates/integrator/src/tests.rs` - `FakeBundleSink`, `FakeWorks`, `FakeTicks`.
- `crates/agents/src/reviewer/tests.rs` - `CollectingSink: BundleUpdateSink`.
- `crates/agents/src/implementer/tests.rs` - `CollectingSink: BundleSink`.
- `crates/agents/src/dispatch/tests.rs` - test `ToolExecutor` impl.
- `crates/agents/tests/seam_implementer.rs`, `seam_reviewer_concurrency.rs`, `seam_implementer_then_reviewer.rs`, `scoped_staging.rs`, `instrumentation.rs` - integration-test fakes for `BundleSink`, `BundleUpdateSink`, and `ToolExecutor`.

Each follows the same pattern: replace `fn method<'a>(&'a self, ...) -> impl Future<Output = ...> + Send + 'a` with `async fn method(&self, ...) -> ...`, drop the `#[allow(clippy::manual_async_fn)]`, leave the body unchanged.

Out-of-scope async-trait sites that remain manually desugared (intentional - they are not the `+ Send` desugaring case):

- `tools::Tool::execute` is a static method (no `&self`, no lifetime); already `impl Future<...> + Send` with no lifetime annotation. Converting it to `async fn` would require `trait_variant` only for cosmetic reasons and is deferred unless a future change makes the static-method form awkward.

### Implementation Plan

The work is one mechanical pattern applied 52 times. Phases group the changes by crate so each phase compiles, tests, and commits independently. Each phase ends with `otto ci` at the repo root running green before the next phase starts.

#### Phase 1: Add `trait-variant` to the workspace, convert `BundleUpdateSink` end-to-end as the spike
**Model:** sonnet

- Run `cargo add -p store trait-variant` to add the dep at the crate level. Once compiling, promote to `[workspace.dependencies]` in the root `Cargo.toml` per the existing convention; in each consuming crate, switch to `trait-variant.workspace = true`.
- Convert `BundleUpdateSink` in `crates/store/src/bundles.rs`:
  - Replace the `#[allow(clippy::manual_async_fn)]` + manual desugaring with `#[trait_variant::make(Send)]` over a `pub trait BundleUpdateSink: Send + Sync { async fn update(&self, ...) -> ... }` body.
  - Convert the three impls (`Store`, `&B`, `Arc<B>`) to use `async fn update(&self, ...)`. Drop the `#[allow]` attributes. Async block bodies are unchanged.
- Convert the `SummaryFanout<S>: BundleUpdateSink` impl in `crates/loopr/src/daemon/summary_fanout.rs`.
- Convert the `CollectingSink: BundleUpdateSink` test fake in `crates/agents/src/reviewer/tests.rs` and any matching `BundleUpdateSink` test fakes in `crates/store/src/bundles/tests.rs`, `crates/agents/tests/seam_*.rs`.
- `cargo check --workspace` and `cargo test -p store -p loopr -p agents`. `otto ci` at the repo root.
- Commit. Single trait, full chain converted; the diff is the reference for every subsequent phase.

#### Phase 2: Convert remaining store sinks and their decorator/fake impls
**Model:** sonnet

- Apply the spike pattern to `WorkUpdateSink` (`crates/store/src/works.rs`) and `PlanUpdateSink` (`crates/store/src/plans.rs`).
- For each: convert trait def, `Store` impl, `&T` impl, `Arc<T>` impl, the `SummaryFanout<S>` impl in `loopr`, and any test fakes.
- Commit per trait so regressions bisect cleanly. `otto ci` after each commit.

#### Phase 3: Convert `integrator` traits and test fakes
**Model:** sonnet

- Convert `WorkLookup` and `TickSink` in `crates/integrator/src/lib.rs`. These have only `Store` and `&T` forwarding (no `Arc<T>`).
- Convert `FakeBundleSink: BundleUpdateSink`, `FakeWorks: WorkLookup`, `FakeTicks: TickSink` in `crates/integrator/src/tests.rs`.
- `cargo test -p integrator`. `otto ci`. Commit.

#### Phase 4: Convert `BundleSink` in agents
**Model:** sonnet

- Convert `BundleSink` in `crates/agents/src/implementer.rs` (trait def + `Store` / `&B` / `Arc<B>` impls).
- Convert `CollectingSink: BundleSink` in `crates/agents/src/implementer/tests.rs`.
- Convert any `BundleSink` test fakes in `crates/agents/tests/seam_*.rs`.
- `cargo test -p agents`. `otto ci`. Commit.

#### Phase 5: Convert `LlmClient` and its decorator/stub
**Model:** sonnet

- Convert `LlmClient` trait in `crates/llm/src/client.rs`. Two methods: `complete_with_tool` and `complete_free`. Both follow the same pattern.
- Convert `AnthropicClient` impl in `crates/llm/src/anthropic.rs`.
- Convert `MeteredLlmClient<L>` decorator impl in `crates/llm/src/metered.rs` (this wraps another `LlmClient`; the inner `Send` bound carries through).
- Convert `StubLlm` impl in `crates/llm/src/stub.rs`.
- `cargo test -p llm`. `otto ci`. Commit. The `tests/span.rs` snapshot tests in `llm` will catch any change in span shape; they should not fire because span emission is on the inherent methods, not the trait.

#### Phase 6: Convert `ToolExecutor` and its test fakes
**Model:** sonnet

- Convert `ToolExecutor` trait in `crates/agents/src/dispatch.rs`. The real impl (the `LaneRouter`-backed one) and any test impl in `crates/agents/src/dispatch/tests.rs` follow.
- Convert any `ToolExecutor` test fakes in `crates/agents/tests/seam_*.rs`, `tests/scoped_staging.rs`, `tests/instrumentation.rs`.
- `cargo test -p agents`. `otto ci`. Commit.

#### Phase 7: Final cleanup and clippy gate
**Model:** sonnet

- Grep the workspace for `#[allow(clippy::manual_async_fn)]`. The remaining occurrences (if any) should be on non-trait async closures or inherent `impl` blocks unrelated to the desugaring pattern; if any are still on a converted trait's site, that conversion was incomplete.
- Run `cargo clippy --workspace --all-targets -- -D warnings`. The `manual_async_fn` lint should fire on zero sites.
- Run `otto ci` at the repo root. All seam tests pass.
- Final commit. The 50-ish lifetime annotations across the affected files are gone; the `<'a>(&'a self, ...) -> impl Future<...> + Send + 'a` pattern is gone; the trait declarations read like normal async Rust.
- Bump per `feedback-bump-always-dash-a.md` (use `/bump` skill, not bare `bump`).

## Alternatives Considered

### Alternative 1: Bring back `async_trait`
- **Description:** Re-add the `async-trait` crate; revert to `#[async_trait] pub trait BundleUpdateSink { async fn update(&self, ...); }`.
- **Pros:** Familiar pattern; ergonomic at definition time.
- **Cons:** Boxes every returned future on every call. Infrastructure-tier traits like `BundleUpdateSink` and `WorkUpdateSink` are called on every FSM transition. The previous async refactor explicitly removed `async_trait` to drop these allocations.
- **Why not chosen:** Reintroduces the heap-allocation overhead the codebase already paid effort to remove.

### Alternative 2: Bare `async fn` in traits, no `Send` bound
- **Description:** Use stable RPITIT (`async fn` in traits, since 1.75) with no `+ Send` bound. Accept that some impls will produce non-`Send` futures.
- **Pros:** Cleanest possible syntax; zero macro magic.
- **Cons:** The daemon spawns work onto multi-threaded tokio; futures must be `Send`. Without the `Send` bound, `tokio::spawn` of any caller of these methods fails to compile. Effectively non-functional for this codebase.
- **Why not chosen:** Doesn't satisfy the `Send` requirement that every existing caller relies on.

### Alternative 3: `return_type_notation` (RTN)
- **Description:** Use the nightly `#![feature(return_type_notation)]` syntax: `where T::method(..): Send`. This is the long-term Rust project plan.
- **Pros:** Most native solution; no macro at all.
- **Cons:** Nightly-only. The workspace has no `rust-toolchain.toml` pin and tracks the user's stable installation; switching to nightly across the workspace for one ergonomic improvement is a large blast radius for a small win.
- **Why not chosen:** Nightly dependency cost is too high; adopt RTN when it stabilizes.

### Alternative 4: Status quo - keep the manual desugaring
- **Description:** Leave the 30-40 sites of `<'a>(&'a self, ...) -> impl Future<...> + Send + 'a` exactly as they are.
- **Pros:** Zero work. Zero risk of breaking the build.
- **Cons:** Every new trait pays the same boilerplate tax. New consumers (the validation work in `2026-05-08-validation.md` will add a `ToolExecutor` injection point following the same pattern) propagate the pattern further. The `#[allow(clippy::manual_async_fn)]` attribute fleet keeps growing.
- **Why not chosen:** The cost is paid forever; the cleanup is mechanical and one-time.

## Technical Considerations

### Dependencies

Add `trait-variant` to `[workspace.dependencies]` via `cargo add -p store trait-variant`, then promote to the root `Cargo.toml` block per the existing convention documented at the top of that file ("Versions were discovered via `cargo add -p <crate> <dep>` on a consumer crate, then promoted here"). Per memory rules ("Never pin cargo add"), do not pin a version - use whatever `cargo add` resolves.

`trait-variant` is maintained by the rust-lang org (github.com/rust-lang/impl-trait-utils). It's the Rust project's official answer to this exact problem.

### Performance

Zero runtime change. The macro expands to the same `<'a>(&'a self, ...) -> impl Future<...> + Send + 'a` form the codebase has hand-written today. Compile times grow by one proc-macro expansion per trait (six new expansions); the impact on workspace build time is negligible.

### Security

No security implications. The change is syntactic.

### Testing Strategy

- The seam tests for each affected crate (`tests/instrumentation.rs`, `tests/bundles.rs`, `tests/works.rs`, `tests/plans.rs` in `store`; `tests/integrate_seam.rs`, `tests/validation_wiring.rs` in `integrator`) exercise the wired paths under the daemon's threading model. If any conversion drops `Send` on a returned future, `tokio::spawn` calls in these tests fail to compile.
- The `Send + Sync` assert tests in `crates/store/tests/{bundles,works,plans}.rs` (`assert_send_sync::<BundlesStore<'static>>()` etc.) confirm the trait objects remain `Send + Sync`.
- `cargo clippy --workspace --all-targets -- -D warnings` after Phase 3 confirms `manual_async_fn` no longer fires on any converted trait.
- `otto ci` at the repo root runs every test in the workspace, including the seam tests.

### Rollout Plan

Three phases, each landing on the `v5` branch as a separate commit. Each phase compiles, tests, and runs `otto ci` clean before advancing. Bumping happens per `feedback-bump-always-dash-a.md` once Phase 3 lands and the workspace is green; the bump is included in the same shipping cadence as the validation work.

This is not a behavior change. There is no feature flag, no rollout gate, no monitoring window. The compile-time / `otto ci` green signal is the rollout signal.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `trait_variant::make` emits subtly different code than the hand-written desugaring | Low | Med | Phase 1 spikes one trait end-to-end and runs the seam tests under tokio's multi-threaded runtime; if expansion differs, the spike catches it before Phases 2 and 3. |
| A test fake or test impl is missed during conversion, leaving the workspace in a half-converted state | Med | Low | Phase 3 grep for `#[allow(clippy::manual_async_fn)]` catches every site that was supposed to be converted. |
| `trait-variant` introduces a transitive dep with an MSRV the workspace doesn't meet | Low | Low | The crate is rust-lang/impl-trait-utils; its MSRV tracks stable minus a few. Workspace is on edition 2024 with a recent stable; `cargo check` will reveal any MSRV gap immediately. |
| Macro hygiene breaks an `instrument` attribute or other proc-macro stacked on the trait method | Low | Med | The current methods carry `#[instrument(...)]` on the impls (not the trait). Phase 1 spike confirms the macro stack composes cleanly; if it doesn't, fall back to keeping `#[instrument]` only on the inherent callees, not the trait impls (the spans on `BundlesStore::update` etc. are sufficient). |
| Macro emits a sibling `Local*` trait that pollutes rustdoc | None | n/a | The in-place `#[trait_variant::make(Send)]` form does not emit a sibling trait; the only generated artifact is the rewritten public trait. (This row stays as a documented constraint: if a future consumer needs the sibling form, the cosmetic concern revives.) |

## Open Questions

- [ ] Does `#[trait_variant::make(Trait: Send)]` compose cleanly with `#[allow(clippy::manual_async_fn)]` removal in one mechanical pass, or does the macro emit code that re-trips the lint? (Validate during Phase 1 spike.)
- [ ] Confirm `trait-variant` does not require a feature flag on stable Rust as of the workspace's current toolchain. (Validate during Phase 1 by running `cargo add` and reading the resolved version's docs.)

## References

- [bucket-3 conversation context: lifetime annotation analysis](../../crates/integrator/src/lib.rs#L40)
- [Memory: async refactor complete, async_trait removed](../../../.claude/projects/-home-saidler-repos-scottidler-loopr/memory/project-async-refactor.md)
- [trait-variant on github.com/rust-lang/impl-trait-utils](https://github.com/rust-lang/impl-trait-utils)
- [Rust async working group note on `Send` bounds](https://blog.rust-lang.org/inside-rust/2023/05/03/stabilizing-async-fn-in-trait.html)
