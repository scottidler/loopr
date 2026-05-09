use std::str::FromStr;

use taskstore_async::{AsyncStore, Filter, FilterOp, IndexValue};
use tokio::sync::Mutex;
use tracing::instrument;

use domain::{PlanId, Work, WorkId};

use crate::error::StoreError;

pub struct WorksStore<'a> {
    inner: &'a AsyncStore,
    update_lock: &'a Mutex<()>,
}

impl<'a> WorksStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore, update_lock: &'a Mutex<()>) -> Self {
        Self { inner, update_lock }
    }

    /// Persist a new Work. Errors with `AlreadyExists` if a work with the
    /// same id is already stored. Mirrors `PlansStore::create` including the
    /// race-condition caveat documented there — pre-check is not
    /// transactional, but freshly minted `WorkId`s make collisions ~0 under
    /// the single-daemon model.
    #[instrument(
        name = "works.create",
        level = "debug",
        skip_all,
        fields(record_kind = "work", record_id = %work.id, parent_id = %work.parent_id, op = "create"),
        ret,
        err,
    )]
    pub async fn create(&self, work: Work) -> Result<WorkId, StoreError> {
        let id_str = work.id.as_ref().to_string();
        if self.inner.get::<Work>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "works",
                id: id_str,
            });
        }
        let returned = self.inner.create(work).await?;
        Ok(WorkId::from_str(&returned).expect("WorkId::from_str is Infallible"))
    }

    /// Batch-create. Delegates to `AsyncStore::create_many`, which inserts
    /// the SQLite cache rows and appends the JSONL truth lines as a single
    /// dispatched writer operation. Per scope memo D10, atomic cross-batch
    /// persistence across *different* JSONL files (e.g. plans + works) is
    /// Stage 7's reconcile-on-restart concern; within a single collection
    /// the writer-thread primitive is atomic.
    ///
    /// A fresh decomposition never has id collisions because `WorkId`s are
    /// freshly minted by the decomposer, so this method's practical failure
    /// mode is limited to IO errors on disk.
    #[instrument(
        name = "works.create_many",
        level = "debug",
        skip_all,
        fields(record_kind = "work", op = "create_many", count = works.len()),
        err,
    )]
    pub async fn create_many(&self, works: Vec<Work>) -> Result<Vec<WorkId>, StoreError> {
        let returned = self.inner.create_many(works).await?;
        Ok(returned
            .into_iter()
            .map(|s| WorkId::from_str(&s).expect("WorkId::from_str is Infallible"))
            .collect())
    }

    /// Fetch a Work by id. Missing id yields `StoreError::RecordNotFound`.
    #[instrument(
        name = "works.get",
        level = "debug",
        skip_all,
        fields(record_kind = "work", record_id = %id, op = "get"),
        err,
    )]
    pub async fn get(&self, id: &WorkId) -> Result<Work, StoreError> {
        match self.inner.get::<Work>(id.as_ref()).await? {
            Some(work) => Ok(work),
            None => Err(StoreError::RecordNotFound {
                collection: "works",
                id: id.to_string(),
            }),
        }
    }

    /// Return every stored Work. `AsyncStore::list` orders by `updated_at`
    /// descending; callers should not depend on order beyond that contract.
    #[instrument(
        name = "works.list",
        level = "debug",
        skip_all,
        fields(record_kind = "work", op = "list", count = tracing::field::Empty),
        err,
    )]
    pub async fn list(&self) -> Result<Vec<Work>, StoreError> {
        let result = self.inner.list::<Work>(&[]).await?;
        tracing::Span::current().record("count", result.len());
        Ok(result)
    }

    /// Corruption-tolerant list. Reads the JSONL files directly,
    /// bypassing the SQLite cache, and returns every line that
    /// parsed plus a sidecar list of every line that did not.
    ///
    /// Used by the daemon's reconcile sweep so that a JSONL-malformed
    /// Work surfaces as a `CorruptionEntry` instead of being silently
    /// dropped at `sync()` (the SQLite cache path) and showing up as
    /// `Ok(None)` from a subsequent `get(work_id)`.
    #[instrument(
        name = "works.list_tolerant",
        level = "debug",
        skip_all,
        fields(
            record_kind = "work",
            op = "list_tolerant",
            count = tracing::field::Empty,
            corruption_count = tracing::field::Empty,
        ),
        err,
    )]
    pub async fn list_tolerant(&self, filters: &[Filter]) -> Result<taskstore_async::ListResult<Work>, StoreError> {
        let result = self.inner.list_tolerant::<Work>(filters).await?;
        tracing::Span::current().record("count", result.records.len());
        tracing::Span::current().record("corruption_count", result.corruption.len());
        Ok(result)
    }

    /// Return every Work whose `parent_id == plan_id`. Backed by the SQLite
    /// index on `parent_id` (`#[record(indexed)]` on the struct field).
    /// Ordered by `updated_at` descending per `AsyncStore::list`'s contract.
    /// Consumed by the Stage 8 wiring capstone's Plan-level completion check
    /// in `spawn_integrator_for_bundle`: after a successful integration,
    /// enumerate sibling Works to decide whether the parent Plan can
    /// transition to `Complete`.
    #[instrument(
        name = "works.list_by_parent_id",
        level = "debug",
        skip_all,
        fields(record_kind = "work", parent_id = %plan_id, op = "list_by_parent_id", count = tracing::field::Empty),
        err,
    )]
    pub async fn list_by_parent_id(&self, plan_id: &PlanId) -> Result<Vec<Work>, StoreError> {
        let filter = Filter {
            field: "parent_id".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(plan_id.to_string()),
        };
        let result = self.inner.list::<Work>(&[filter]).await?;
        tracing::Span::current().record("count", result.len());
        Ok(result)
    }

    /// Persist a status / field change on an existing Work with
    /// intra-daemon optimistic concurrency control.
    ///
    /// Sequence under the lock:
    ///
    /// 1. acquire `update_lock`,
    /// 2. read the current Work by id; missing id -> `RecordNotFound`,
    /// 3. compare the on-disk `updated_at` to `expected_updated_at`;
    ///    mismatch -> `StoreError::Stale { expected, actual }`,
    /// 4. write the new Work via `AsyncStore::update`,
    /// 5. drop the lock.
    ///
    /// This closes the concurrent-promotion race where two sibling
    /// Works go Done simultaneously, both find the same Pending Work
    /// eligible, and both call `spawn_implementer_for_work`. The first
    /// writer commits; the second sees a mismatched `updated_at` and
    /// returns `Stale` without spawning a second Implementer.
    #[instrument(
        name = "works.update",
        level = "debug",
        skip_all,
        fields(record_kind = "work", record_id = %work.id, status = ?work.status, expected_updated_at, op = "update"),
        ret,
        err,
    )]
    pub async fn update(&self, work: Work, expected_updated_at: i64) -> Result<(), StoreError> {
        let _guard = self.update_lock.lock().await;
        let id_str = work.id.as_ref().to_string();
        let current = self
            .inner
            .get::<Work>(&id_str)
            .await?
            .ok_or(StoreError::RecordNotFound {
                collection: "works",
                id: id_str,
            })?;
        if current.updated_at != expected_updated_at {
            return Err(StoreError::Stale {
                expected: expected_updated_at,
                actual: current.updated_at,
            });
        }
        self.inner.update(work).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// `WorkUpdateSink` trait + impls
//
// Mirrors `BundleUpdateSink`'s shape exactly. OCC added in the
// dependency-gate design (docs/design/2026-05-07-dependency-gate.md
// Phase 0) to prevent double Implementer spawning when two sibling
// Works go Done concurrently and both find the same Pending Work
// eligible for promotion.
// ---------------------------------------------------------------------------

/// Minimal Work-update interface for sink-generic transition helpers.
/// Passes the OCC `expected_updated_at` snapshot the caller took
/// before mutating its clone.
#[trait_variant::make(Send)]
pub trait WorkUpdateSink: Send + Sync {
    async fn update(&self, work: Work, expected_updated_at: i64) -> Result<(), WorkUpdateError>;
}

#[derive(Debug, thiserror::Error)]
pub enum WorkUpdateError {
    #[error("work update failed: {0}")]
    Update(String),
    /// OCC version-check failure. Callers in the concurrent-promotion
    /// path should treat this as a benign race (the Work was already
    /// advanced by another task) and return without error.
    #[error("stale work: expected updated_at={expected}, actual={actual}")]
    Stale { expected: i64, actual: i64 },
}

/// Real `WorkUpdateSink` backed by `store::Store`. Delegates to
/// `WorksStore::update` which holds the intra-daemon OCC Mutex;
/// `StoreError::Stale` is preserved as `WorkUpdateError::Stale` so
/// downstream matching works.
impl WorkUpdateSink for crate::Store {
    async fn update(&self, work: Work, expected_updated_at: i64) -> Result<(), WorkUpdateError> {
        match self.works().update(work, expected_updated_at).await {
            Ok(()) => Ok(()),
            Err(StoreError::Stale { expected, actual }) => Err(WorkUpdateError::Stale { expected, actual }),
            Err(other) => Err(WorkUpdateError::Update(other.to_string())),
        }
    }
}

/// Forwarding impl for any reference to a `WorkUpdateSink`. Mirrors
/// `BundleUpdateSink`'s borrowed-store helper.
impl<W: WorkUpdateSink + ?Sized> WorkUpdateSink for &W {
    async fn update(&self, work: Work, expected_updated_at: i64) -> Result<(), WorkUpdateError> {
        (*self).update(work, expected_updated_at).await
    }
}

/// Forwarding impl for `Arc<W>`. Lets the daemon construct
/// `SummaryFanout::new(Arc::clone(&store), ...)` without unwrapping.
impl<W: WorkUpdateSink + ?Sized> WorkUpdateSink for std::sync::Arc<W> {
    async fn update(&self, work: Work, expected_updated_at: i64) -> Result<(), WorkUpdateError> {
        (**self).update(work, expected_updated_at).await
    }
}
