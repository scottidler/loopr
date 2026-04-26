use std::future::Future;
use std::str::FromStr;

use taskstore_async::AsyncStore;
use tracing::instrument;

use domain::{Plan, PlanId, Work};

use crate::error::StoreError;

pub struct PlansStore<'a> {
    inner: &'a AsyncStore,
}

impl<'a> PlansStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore) -> Self {
        Self { inner }
    }

    /// Persist a new Plan. Errors with `AlreadyExists` if a plan with the
    /// same id is already stored.
    ///
    /// This enforces the domain contract of `create` (no-overwrite) at the
    /// anti-corruption boundary. The underlying `taskstore_async::AsyncStore`
    /// inherits SQLite's `INSERT OR REPLACE` semantics and would silently
    /// overwrite; the pre-check `get` here converts that into an explicit
    /// `StoreError::AlreadyExists`.
    ///
    /// **Race-condition caveat:** the pre-check is not transactional. A
    /// concurrent create of the same id between the `get` and the upstream
    /// `create` would still overwrite. Stage 5's single-daemon model mints
    /// fresh random `PlanId`s via `Plan::new()`, making collision ~0. When a
    /// multi-writer scenario emerges, extend `AsyncStore` with a conditional-
    /// write primitive and replace this pre-check with the atomic upstream
    /// path.
    #[instrument(
        name = "plans.create",
        level = "debug",
        skip_all,
        fields(record_kind = "plan", record_id = %plan.id, op = "create", goal_len = plan.goal.len()),
        ret,
        err,
    )]
    pub async fn create(&self, plan: Plan) -> Result<PlanId, StoreError> {
        let id_str = plan.id.as_ref().to_string();
        if self.inner.get::<Plan>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "plans",
                id: id_str,
            });
        }
        let returned = self.inner.create(plan).await?;
        Ok(PlanId::from_str(&returned).expect("PlanId::from_str is Infallible"))
    }

    /// Fetch a Plan by id. Missing id yields `StoreError::RecordNotFound`;
    /// the `Option<T>` from the underlying store is collapsed here so every
    /// Stage 5+ accessor returns the same shape.
    #[instrument(
        name = "plans.get",
        level = "debug",
        skip_all,
        fields(record_kind = "plan", record_id = %id, op = "get"),
        err,
    )]
    pub async fn get(&self, id: &PlanId) -> Result<Plan, StoreError> {
        match self.inner.get::<Plan>(id.as_ref()).await? {
            Some(plan) => Ok(plan),
            None => Err(StoreError::RecordNotFound {
                collection: "plans",
                id: id.to_string(),
            }),
        }
    }

    /// Return every stored Plan. `AsyncStore::list` orders by `updated_at`
    /// descending; callers should not depend on order beyond that contract.
    #[instrument(
        name = "plans.list",
        level = "debug",
        skip_all,
        fields(record_kind = "plan", op = "list", count = tracing::field::Empty),
        err,
    )]
    pub async fn list(&self) -> Result<Vec<Plan>, StoreError> {
        let result = self.inner.list::<Plan>(&[]).await?;
        tracing::Span::current().record("count", result.len());
        Ok(result)
    }

    /// Persist a status / field change on an existing Plan. Delegates to
    /// `AsyncStore::update`, which rewrites the JSONL line and refreshes the
    /// SQLite cache row. Consumed by the Stage 8 wiring capstone's Integrator
    /// spawn: after every child Work under a Plan is terminal with at least
    /// one Done, the Coordinator fires `Active -> Complete` via this method.
    /// Mirrors `WorksStore::update` (blind-write, no OCC); Plans have no
    /// concurrent-writer race in the single-daemon-per-target threat model.
    #[instrument(
        name = "plans.update",
        level = "debug",
        skip_all,
        fields(record_kind = "plan", record_id = %plan.id, op = "update"),
        ret,
        err,
    )]
    pub async fn update(&self, plan: Plan) -> Result<(), StoreError> {
        self.inner.update(plan).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// `PlanUpdateSink` trait + impls
//
// Phase 5 of the Tier-1 cleanup. The Plan summary renderer needs both
// the Plan and its child Works (`summary::write_plan(target, plan,
// &children)`). Per design Alternatives §4 option (c-extended), the
// caller fetches children before invoking `update`; the decorator
// forwards both args into the renderer. Plan updates are not OCC-
// tracked today (only Bundle is), so the trait surface omits
// `expected_updated_at`.
// ---------------------------------------------------------------------------

/// Minimal Plan-update interface for sink-generic transition helpers.
/// Carries `children` so the `SummaryFanout` decorator's `PlanUpdateSink`
/// impl can render the Plan summary without the decorator gaining a
/// concrete-store dependency. The real impl on `Store` ignores
/// `children` for persistence (only `plan` is updated via
/// `PlansStore::update`).
#[allow(clippy::manual_async_fn)]
pub trait PlanUpdateSink: Send + Sync {
    fn update<'a>(
        &'a self,
        plan: Plan,
        children: Vec<Work>,
    ) -> impl Future<Output = Result<(), PlanUpdateError>> + Send + 'a;
}

#[derive(Debug, thiserror::Error)]
pub enum PlanUpdateError {
    #[error("plan update failed: {0}")]
    Update(String),
}

/// Real `PlanUpdateSink` backed by `store::Store`. Delegates to
/// `PlansStore::update`; `children` is unused at this layer (the
/// decorator's render path uses it).
impl PlanUpdateSink for crate::Store {
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(
        &'a self,
        plan: Plan,
        _children: Vec<Work>,
    ) -> impl Future<Output = Result<(), PlanUpdateError>> + Send + 'a {
        async move {
            self.plans()
                .update(plan)
                .await
                .map_err(|e| PlanUpdateError::Update(e.to_string()))
        }
    }
}

/// Forwarding impl for any reference to a `PlanUpdateSink`.
impl<P: PlanUpdateSink + ?Sized> PlanUpdateSink for &P {
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(
        &'a self,
        plan: Plan,
        children: Vec<Work>,
    ) -> impl Future<Output = Result<(), PlanUpdateError>> + Send + 'a {
        async move { (*self).update(plan, children).await }
    }
}

/// Forwarding impl for `Arc<P>`.
impl<P: PlanUpdateSink + ?Sized> PlanUpdateSink for std::sync::Arc<P> {
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(
        &'a self,
        plan: Plan,
        children: Vec<Work>,
    ) -> impl Future<Output = Result<(), PlanUpdateError>> + Send + 'a {
        async move { (**self).update(plan, children).await }
    }
}
