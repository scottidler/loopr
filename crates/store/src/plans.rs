use std::str::FromStr;

use taskstore_async::AsyncStore;
use tokio::sync::Mutex;
use tracing::instrument;

use domain::{Plan, PlanId, Work};

use crate::error::StoreError;

pub struct PlansStore<'a> {
    inner: &'a AsyncStore,
    update_lock: &'a Mutex<()>,
}

impl<'a> PlansStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore, update_lock: &'a Mutex<()>) -> Self {
        Self { inner, update_lock }
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

    /// Persist a status / field change on an existing Plan with
    /// intra-daemon optimistic concurrency control.
    ///
    /// Sequence under the lock (mirrors `WorksStore::update` /
    /// `BundlesStore::update`):
    ///
    /// 1. acquire `update_lock`,
    /// 2. read the current Plan by id; missing id -> `RecordNotFound`
    ///    (the underlying taskstore `update` is an upsert and would
    ///    silently CREATE a missing id; this pre-`get` converts that into
    ///    an explicit error, matching Works/Bundles),
    /// 3. compare on-disk `updated_at` to `expected_updated_at`;
    ///    mismatch -> `StoreError::Stale { expected, actual }`,
    /// 4. floor `updated_at` strictly above the prior value and write,
    /// 5. drop the lock; return the floored `updated_at`.
    ///
    /// Plans have three live concurrent writers (the Reactor's
    /// `transition_and_persist_plan`, the Director's `Stalled` persist, the
    /// IPC override handler). Without OCC an interleaving writes
    /// FSM-invalid history (a terminal `Complete` overwritten by a late
    /// `Stalled`). The returned floored `updated_at` lets a caller chaining
    /// a second transition refresh its expected version.
    #[instrument(
        name = "plans.update",
        level = "debug",
        skip_all,
        fields(record_kind = "plan", record_id = %plan.id, status = ?plan.status, expected_updated_at, op = "update"),
        ret,
        err,
    )]
    pub async fn update(&self, mut plan: Plan, expected_updated_at: i64) -> Result<i64, StoreError> {
        let _guard = self.update_lock.lock().await;
        let id_str = plan.id.as_ref().to_string();
        let current = self
            .inner
            .get::<Plan>(&id_str)
            .await?
            .ok_or(StoreError::RecordNotFound {
                collection: "plans",
                id: id_str,
            })?;
        if current.updated_at != expected_updated_at {
            return Err(StoreError::Stale {
                expected: expected_updated_at,
                actual: current.updated_at,
            });
        }
        let floored = std::cmp::max(domain::now_millis(), current.updated_at + 1);
        plan.updated_at = floored;
        self.inner.update(plan).await?;
        Ok(floored)
    }
}

// ---------------------------------------------------------------------------
// `PlanUpdateSink` trait + impls
//
// Phase 5 of the Tier-1 cleanup. The Plan summary renderer needs both
// the Plan and its child Works (`summary::write_plan(target, plan,
// &children)`). Per design Alternatives §4 option (c-extended), the
// caller fetches children before invoking `update`; the decorator
// forwards both args into the renderer.
//
// Phase 3 of the code-review remediation added OCC to Plans (three live
// concurrent writers), so the trait now carries `expected_updated_at`
// and surfaces `Stale` separately — mirroring `BundleUpdateSink` /
// `WorkUpdateSink`. On success it returns the persisted floored
// `updated_at`.
// ---------------------------------------------------------------------------

/// Minimal Plan-update interface for sink-generic transition helpers.
/// Carries `children` so the `SummaryFanout` decorator's `PlanUpdateSink`
/// impl can render the Plan summary without the decorator gaining a
/// concrete-store dependency. The real impl on `Store` ignores
/// `children` for persistence (only `plan` is updated via
/// `PlansStore::update`). Passes the OCC `expected_updated_at` snapshot
/// the caller took before mutating its clone; returns the persisted
/// floored `updated_at`.
#[trait_variant::make(Send)]
pub trait PlanUpdateSink: Send + Sync {
    async fn update(&self, plan: Plan, children: Vec<Work>, expected_updated_at: i64) -> Result<i64, PlanUpdateError>;
}

#[derive(Debug, thiserror::Error)]
pub enum PlanUpdateError {
    #[error("plan update failed: {0}")]
    Update(String),
    /// OCC version-check failure. Callers in a concurrent-writer path
    /// (Reactor vs Director vs IPC override) should treat this as a
    /// benign race (the Plan was already advanced by another writer) and
    /// return without clobbering.
    #[error("stale plan: expected updated_at={expected}, actual={actual}")]
    Stale { expected: i64, actual: i64 },
}

/// Real `PlanUpdateSink` backed by `store::Store`. Delegates to
/// `PlansStore::update`; `children` is unused at this layer (the
/// decorator's render path uses it). `StoreError::Stale` is preserved as
/// `PlanUpdateError::Stale` so downstream matching works.
impl PlanUpdateSink for crate::Store {
    async fn update(&self, plan: Plan, _children: Vec<Work>, expected_updated_at: i64) -> Result<i64, PlanUpdateError> {
        match self.plans().update(plan, expected_updated_at).await {
            Ok(ts) => Ok(ts),
            Err(StoreError::Stale { expected, actual }) => Err(PlanUpdateError::Stale { expected, actual }),
            Err(other) => Err(PlanUpdateError::Update(other.to_string())),
        }
    }
}

/// Forwarding impl for any reference to a `PlanUpdateSink`.
impl<P: PlanUpdateSink + ?Sized> PlanUpdateSink for &P {
    async fn update(&self, plan: Plan, children: Vec<Work>, expected_updated_at: i64) -> Result<i64, PlanUpdateError> {
        (*self).update(plan, children, expected_updated_at).await
    }
}

/// Forwarding impl for `Arc<P>`.
impl<P: PlanUpdateSink + ?Sized> PlanUpdateSink for std::sync::Arc<P> {
    async fn update(&self, plan: Plan, children: Vec<Work>, expected_updated_at: i64) -> Result<i64, PlanUpdateError> {
        (**self).update(plan, children, expected_updated_at).await
    }
}
