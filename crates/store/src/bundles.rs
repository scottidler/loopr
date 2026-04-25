use std::future::Future;
use std::str::FromStr;

use taskstore_async::{AsyncStore, Filter, FilterOp, IndexValue};
use tokio::sync::Mutex;
use tracing::instrument;

use domain::{Bundle, BundleId, WorkId};

use crate::error::StoreError;

pub struct BundlesStore<'a> {
    inner: &'a AsyncStore,
    update_lock: &'a Mutex<()>,
}

impl<'a> BundlesStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore, update_lock: &'a Mutex<()>) -> Self {
        Self { inner, update_lock }
    }

    /// Persist a new Bundle. Errors with `AlreadyExists` if a bundle
    /// with the same id is already stored. Mirrors `PlansStore::create`
    /// and `WorksStore::create` race-condition caveat: the pre-check
    /// is not transactional, but freshly minted `BundleId`s make
    /// collisions ~0 under the single-daemon model.
    #[instrument(
        name = "bundles.create",
        level = "debug",
        skip_all,
        fields(
            record_kind = "bundle",
            record_id = %bundle.id,
            work_id = %bundle.work_id,
            op = "create",
            force_proposed = bundle.force_proposed,
        ),
        ret,
        err,
    )]
    pub async fn create(&self, bundle: Bundle) -> Result<BundleId, StoreError> {
        let id_str = bundle.id.as_ref().to_string();
        if self.inner.get::<Bundle>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "bundles",
                id: id_str,
            });
        }
        let returned = self.inner.create(bundle).await?;
        Ok(BundleId::from_str(&returned).expect("BundleId::from_str is Infallible"))
    }

    /// Fetch a Bundle by id. Missing id yields `StoreError::RecordNotFound`.
    #[instrument(
        name = "bundles.get",
        level = "debug",
        skip_all,
        fields(record_kind = "bundle", record_id = %id, op = "get"),
        err,
    )]
    pub async fn get(&self, id: &BundleId) -> Result<Bundle, StoreError> {
        match self.inner.get::<Bundle>(id.as_ref()).await? {
            Some(bundle) => Ok(bundle),
            None => Err(StoreError::RecordNotFound {
                collection: "bundles",
                id: id.to_string(),
            }),
        }
    }

    /// Return every stored Bundle. `AsyncStore::list` orders by
    /// `updated_at` descending; callers should not depend on order
    /// beyond that contract.
    #[instrument(
        name = "bundles.list",
        level = "debug",
        skip_all,
        fields(record_kind = "bundle", op = "list", count = tracing::field::Empty),
        err,
    )]
    pub async fn list(&self) -> Result<Vec<Bundle>, StoreError> {
        let result = self.inner.list::<Bundle>(&[]).await?;
        tracing::Span::current().record("count", result.len());
        Ok(result)
    }

    /// Return every Bundle whose `work_id` matches the given
    /// `WorkId`. Backed by the SQLite index on `work_id`
    /// (`#[record(indexed)]` on the struct field), so this is an
    /// index lookup rather than a full-table scan.
    #[instrument(
        name = "bundles.list_by_work_id",
        level = "debug",
        skip_all,
        fields(record_kind = "bundle", work_id = %work_id, op = "list_by_work_id", count = tracing::field::Empty),
        err,
    )]
    pub async fn list_by_work_id(&self, work_id: &WorkId) -> Result<Vec<Bundle>, StoreError> {
        let filter = Filter {
            field: "work_id".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(work_id.to_string()),
        };
        let result = self.inner.list::<Bundle>(&[filter]).await?;
        tracing::Span::current().record("count", result.len());
        Ok(result)
    }

    /// Persist a status / field change on an existing Bundle with
    /// intra-daemon optimistic concurrency control.
    ///
    /// Sequence under the lock:
    ///
    /// 1. acquire `update_lock`,
    /// 2. read the current Bundle by id; missing id -> `RecordNotFound`,
    /// 3. compare the on-disk `updated_at` to `expected_updated_at`;
    ///    mismatch -> `StoreError::Stale { expected, actual }`,
    /// 4. write the new Bundle via `AsyncStore::update`,
    /// 5. drop the lock.
    ///
    /// This closes the race where two Reviewer tasks in one daemon
    /// both read a Bundle, both mutate (in memory), and both write:
    /// the first winner commits, the second winner sees the updated
    /// `updated_at` and returns `Stale` without clobbering the first
    /// winner's `verification` + status.
    ///
    /// Cross-process OCC is not in scope. Loopr's threat model is
    /// single-daemon-per-target (`.loopr/daemon.pid` lockfile) plus
    /// never-push; multi-daemon or external-writer OCC would need an
    /// upstream `taskstore-async::update_if` CAS primitive, which is
    /// deferred.
    #[instrument(
        name = "bundles.update",
        level = "debug",
        skip_all,
        fields(
            record_kind = "bundle",
            record_id = %bundle.id,
            work_id = %bundle.work_id,
            status = ?bundle.status,
            expected_updated_at,
            op = "update",
        ),
        ret,
        err,
    )]
    pub async fn update(&self, bundle: Bundle, expected_updated_at: i64) -> Result<(), StoreError> {
        let _guard = self.update_lock.lock().await;
        let id_str = bundle.id.as_ref().to_string();
        let current = self
            .inner
            .get::<Bundle>(&id_str)
            .await?
            .ok_or(StoreError::RecordNotFound {
                collection: "bundles",
                id: id_str,
            })?;
        if current.updated_at != expected_updated_at {
            return Err(StoreError::Stale {
                expected: expected_updated_at,
                actual: current.updated_at,
            });
        }
        self.inner.update(bundle).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// `BundleUpdateSink` trait + impls
//
// The agent-side view of `BundlesStore::update`. Lives in `store` (not
// `agents`) so that `integrator` can consume it without pulling `agents`
// (which transitively pulls `llm`, breaking the integrator's Cargo-graph-
// mechanical `llm`-free invariant). Relocated from `crates/agents/src/reviewer.rs`
// per docs/design/2026-04-22-integrator.md; the Reviewer doc's design is
// unchanged at the trait's shape, only its module path.
// ---------------------------------------------------------------------------

/// Minimal Bundle-update interface. Single-method trait so per-role
/// test fakes stay tiny. Passes the OCC `expected_updated_at` snapshot
/// the caller took before mutating its clone.
#[allow(clippy::manual_async_fn)]
pub trait BundleUpdateSink: Send + Sync {
    fn update<'a>(
        &'a self,
        bundle: Bundle,
        expected_updated_at: i64,
    ) -> impl Future<Output = Result<(), BundleUpdateError>> + Send + 'a;
}

#[derive(Debug, thiserror::Error)]
pub enum BundleUpdateError {
    #[error("bundle update failed: {0}")]
    Update(String),
    /// OCC version-check failure from the underlying store. Callers
    /// are expected to match on this variant specifically (Reviewer
    /// drops the losing Verdict silently; Integrator routes to retry).
    #[error("stale bundle: expected updated_at={expected}, actual={actual}")]
    Stale { expected: i64, actual: i64 },
}

/// Real `BundleUpdateSink` backed by `store::Store`. Delegates to
/// `BundlesStore::update` which holds the intra-daemon OCC Mutex;
/// `StoreError::Stale` is specifically preserved as
/// `BundleUpdateError::Stale` so downstream matching works.
impl BundleUpdateSink for crate::Store {
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(
        &'a self,
        bundle: Bundle,
        expected_updated_at: i64,
    ) -> impl Future<Output = Result<(), BundleUpdateError>> + Send + 'a {
        async move {
            match self.bundles().update(bundle, expected_updated_at).await {
                Ok(()) => Ok(()),
                Err(StoreError::Stale { expected, actual }) => Err(BundleUpdateError::Stale { expected, actual }),
                Err(other) => Err(BundleUpdateError::Update(other.to_string())),
            }
        }
    }
}

/// Forwarding impl for any reference to a `BundleUpdateSink`. Lets
/// callers build deps with a borrowed `Store` without cloning.
impl<B: BundleUpdateSink + ?Sized> BundleUpdateSink for &B {
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(
        &'a self,
        bundle: Bundle,
        expected_updated_at: i64,
    ) -> impl Future<Output = Result<(), BundleUpdateError>> + Send + 'a {
        async move { (*self).update(bundle, expected_updated_at).await }
    }
}

#[cfg(test)]
mod tests;
