//! Deterministic, non-LLM merge-publish. Accepted Bundles into Tick.
//!
//! Entry point: `integrate(bundles, plan, deps) -> Result<Tick, IntegrationError>`.
//! First-gate scope is merge-only; validation is deferred (see the crate's
//! CLAUDE.md and docs/design/2026-04-22-integrator.md).
//!
//! This crate does not depend on `llm` (mechanically enforced at the Cargo
//! graph level; `cargo tree -p integrator -i llm` returns "not found").
//! Agent-specific plumbing (`agents::run_reviewer`, etc.) is out of scope.

mod config;
mod error;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use domain::{Tick, Work};
use store::{BundleUpdateSink, StoreError};

pub use config::IntegratorConfig;
pub use error::IntegrationError;

// ---------------------------------------------------------------------------
// DI traits: the `IntegratorDeps` struct bundles these.
// ---------------------------------------------------------------------------

/// Read-only `Work` lookup. The Integrator fetches Work records by
/// `bundle.work_id` to verify `work.parent_id == plan.id` during
/// pre-flight. Read-only; the Integrator never transitions a Work
/// (that's the daemon's job after an `Ok(Tick)` return).
pub trait WorkLookup: Send + Sync {
    fn get<'a>(&'a self, work_id: &'a str) -> impl Future<Output = Result<Option<Work>, StoreError>> + Send + 'a;
}

/// Append-only `Tick` persistence. The Integrator calls `create` in
/// Phase 3 after the git sequence succeeds. On a duplicate
/// `(plan_id, bundles-as-set)`, the store returns
/// `StoreError::DuplicateTick { tick_id, .. }` and the Integrator
/// promotes to a no-op on the crash-recovery path.
pub trait TickSink: Send + Sync {
    fn create<'a>(&'a self, tick: Tick) -> impl Future<Output = Result<Tick, StoreError>> + Send + 'a;
}

// Real impls backed by `store::Store`.

impl WorkLookup for store::Store {
    #[allow(clippy::manual_async_fn)]
    fn get<'a>(&'a self, work_id: &'a str) -> impl Future<Output = Result<Option<Work>, StoreError>> + Send + 'a {
        async move {
            // `WorksStore::get` returns `Err(RecordNotFound)` for missing;
            // the WorkLookup contract is `Option` so the Integrator can
            // distinguish "wiring bug (bundle references a non-existent
            // work)" from other store failures.
            use domain::WorkId;
            use std::str::FromStr;
            let wid = WorkId::from_str(work_id).expect("WorkId::from_str is Infallible");
            match self.works().get(&wid).await {
                Ok(w) => Ok(Some(w)),
                Err(StoreError::RecordNotFound { .. }) => Ok(None),
                Err(other) => Err(other),
            }
        }
    }
}

impl<T: WorkLookup + ?Sized> WorkLookup for &T {
    #[allow(clippy::manual_async_fn)]
    fn get<'a>(&'a self, work_id: &'a str) -> impl Future<Output = Result<Option<Work>, StoreError>> + Send + 'a {
        async move { (*self).get(work_id).await }
    }
}

impl TickSink for store::Store {
    #[allow(clippy::manual_async_fn)]
    fn create<'a>(&'a self, tick: Tick) -> impl Future<Output = Result<Tick, StoreError>> + Send + 'a {
        async move { self.ticks().create(tick).await }
    }
}

impl<T: TickSink + ?Sized> TickSink for &T {
    #[allow(clippy::manual_async_fn)]
    fn create<'a>(&'a self, tick: Tick) -> impl Future<Output = Result<Tick, StoreError>> + Send + 'a {
        async move { (*self).create(tick).await }
    }
}

// ---------------------------------------------------------------------------
// `IntegratorDeps`: the single handle passed to `integrate`.
// ---------------------------------------------------------------------------

/// Bundles the Integrator's injected dependencies. One generic
/// parameter flows through `integrate`'s signature; concrete trait
/// bounds live on the struct. Matches the `Deps<L, T, W, S, C>`
/// pattern from `agents`, but with a different set of traits:
/// no `LlmClient`, no `ToolExecutor`, no `ContextBuilder`.
pub struct IntegratorDeps<U, W, T>
where
    U: BundleUpdateSink,
    W: WorkLookup,
    T: TickSink,
{
    /// OCC update sink for Bundle transitions (`Accepted -> Integrating`,
    /// `Integrating -> Merged | IntegrationFailed`). Same trait the
    /// Reviewer consumes, relocated from `agents` to `store` in Phase 1.
    pub bundle_sink: U,

    /// Read-only Work lookup for the `work.parent_id == plan.id`
    /// pre-flight check.
    pub works: W,

    /// Append-only Tick persistence for Phase 3.
    pub ticks: T,

    /// Runtime knobs (timeouts, multi-Bundle guardrail).
    pub config: IntegratorConfig,

    /// Target repo root. Used for `git -C <target>` subprocesses.
    pub target: PathBuf,

    /// Intra-daemon working-tree serializer. Held for the full
    /// checkout/merge/rollback sequence (Phase 2). Two parallel
    /// Integrator tasks on the same `Store` share this lock; two
    /// parallel Integrators on different `Store`s (impossible under
    /// single-daemon-per-target) would need separate locks.
    pub git_lock: Arc<Mutex<()>>,
}

// ---------------------------------------------------------------------------
// Entry point (Phase 3 will fill this in).
// ---------------------------------------------------------------------------

/// Merge Accepted Bundles into a Plan's integration branch and
/// produce a Tick. See docs/design/2026-04-22-integrator.md for the
/// full loop contract. Phase 3 lands the body; Phase 2 provides the
/// signature so callers can compile-check against it.
pub async fn integrate<U, W, T>(
    _bundles: &[domain::Bundle],
    _plan: &domain::Plan,
    _deps: &IntegratorDeps<U, W, T>,
) -> Result<Tick, IntegrationError>
where
    U: BundleUpdateSink,
    W: WorkLookup,
    T: TickSink,
{
    // Phase 3: implement. Phase 2 stub returns NoBundles to keep the
    // signature concrete.
    Err(IntegrationError::NoBundles)
}

#[cfg(test)]
mod tests;
