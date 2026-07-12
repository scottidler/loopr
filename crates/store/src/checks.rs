//! `CheckRunsStore`: typed accessor for `CheckRun` records (Phase 7 of
//! `docs/design/2026-07-11-verified-swarm.md`).
//!
//! Mirrors `NotesStore`: JSONL append-only on disk with an SQLite cache
//! for the `bundle_id` index. CheckRuns are immutable evidence — no
//! `update`, no OCC lock. Phase 10 (Reviewer executed checks) is the first
//! writer, persisting one record per executed command; the read path is
//! `list_by_bundle` (indexed `bundle_id` query).

use std::str::FromStr;

use taskstore_async::{AsyncStore, Filter, FilterOp, IndexValue};
use tracing::instrument;

use domain::{BundleId, CheckRun, CheckRunId};

use crate::error::StoreError;

/// Narrow write sink for `CheckRun` evidence, mirroring the `*UpdateSink`
/// pattern (`BundleUpdateSink` et al.). CheckRuns are append-only immutable
/// facts, so this is create-only — no OCC, no update. The Reviewer (Phase 10)
/// and Integrator (Phase 12) persist through this so a caller can inject a
/// real `Store` (or a test fake) without depending on the concrete store type.
/// The `&B` / `Arc<B>` forwarding impls let callers pass a borrowed sink.
#[trait_variant::make(Send)]
pub trait CheckRunSink: Send + Sync {
    async fn create_check_run(&self, check_run: CheckRun) -> Result<CheckRunId, StoreError>;
}

impl CheckRunSink for crate::Store {
    async fn create_check_run(&self, check_run: CheckRun) -> Result<CheckRunId, StoreError> {
        self.check_runs().create(check_run).await
    }
}

impl<B: CheckRunSink + ?Sized> CheckRunSink for &B {
    async fn create_check_run(&self, check_run: CheckRun) -> Result<CheckRunId, StoreError> {
        (*self).create_check_run(check_run).await
    }
}

impl<B: CheckRunSink + ?Sized> CheckRunSink for std::sync::Arc<B> {
    async fn create_check_run(&self, check_run: CheckRun) -> Result<CheckRunId, StoreError> {
        (**self).create_check_run(check_run).await
    }
}

pub struct CheckRunsStore<'a> {
    inner: &'a AsyncStore,
}

impl<'a> CheckRunsStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore) -> Self {
        Self { inner }
    }

    /// Persist a new CheckRun. `AlreadyExists` if a record with the same
    /// id is already stored — vanishingly unlikely with freshly minted
    /// `CheckRunId`s, but the pre-check mirrors the other stores.
    #[instrument(
        name = "checkruns.create",
        level = "debug",
        skip_all,
        fields(
            record_kind = "checkrun",
            record_id = %check_run.id,
            bundle_id = %check_run.bundle_id,
            work_id = %check_run.work_id,
            op = "create",
        ),
        ret,
        err,
    )]
    pub async fn create(&self, check_run: CheckRun) -> Result<CheckRunId, StoreError> {
        let id_str = check_run.id.as_ref().to_string();
        if self.inner.get::<CheckRun>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "checkruns",
                id: id_str,
            });
        }
        let returned = self.inner.create(check_run).await?;
        Ok(CheckRunId::from_str(&returned).expect("CheckRunId::from_str is Infallible"))
    }

    /// Fetch a CheckRun by id. Missing id yields `StoreError::RecordNotFound`.
    #[instrument(
        name = "checkruns.get",
        level = "debug",
        skip_all,
        fields(record_kind = "checkrun", record_id = %id, op = "get"),
        err,
    )]
    pub async fn get(&self, id: &CheckRunId) -> Result<CheckRun, StoreError> {
        match self.inner.get::<CheckRun>(id.as_ref()).await? {
            Some(check_run) => Ok(check_run),
            None => Err(StoreError::RecordNotFound {
                collection: "checkruns",
                id: id.to_string(),
            }),
        }
    }

    /// Every CheckRun for the given Bundle. Backed by the SQLite index on
    /// `bundle_id` (`#[record(indexed)]` on the struct field), so this is
    /// an index lookup rather than a full-table scan.
    #[instrument(
        name = "checkruns.list_by_bundle",
        level = "debug",
        skip_all,
        fields(record_kind = "checkrun", bundle_id = %bundle_id, op = "list_by_bundle", count = tracing::field::Empty),
        err,
    )]
    pub async fn list_by_bundle(&self, bundle_id: &BundleId) -> Result<Vec<CheckRun>, StoreError> {
        let filter = Filter {
            field: "bundle_id".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(bundle_id.to_string()),
        };
        let result = self.inner.list::<CheckRun>(&[filter]).await?;
        tracing::Span::current().record("count", result.len());
        Ok(result)
    }
}
