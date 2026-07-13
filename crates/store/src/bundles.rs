use std::str::FromStr;

use taskstore_async::{AsyncStore, Filter, FilterOp, IndexValue};
use tokio::sync::Mutex;
use tracing::{error, instrument, warn};

use domain::{Bundle, BundleId, BundleStatus, Role, TargetKind, WorkId};

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

    /// Corruption-tolerant list. Reads the JSONL files directly,
    /// bypassing the SQLite cache, and returns every line that
    /// parsed plus a sidecar list of every line that did not.
    ///
    /// Used by the daemon's reconcile sweep so that a corrupt JSONL
    /// row surfaces as a `CorruptionEntry` instead of either failing
    /// the whole sweep (the `list` path) or being silently dropped
    /// (the SQLite cache path that `sync` populates).
    #[instrument(
        name = "bundles.list_tolerant",
        level = "debug",
        skip_all,
        fields(
            record_kind = "bundle",
            op = "list_tolerant",
            count = tracing::field::Empty,
            corruption_count = tracing::field::Empty,
        ),
        err,
    )]
    pub async fn list_tolerant(&self, filters: &[Filter]) -> Result<taskstore_async::ListResult<Bundle>, StoreError> {
        let result = self.inner.list_tolerant::<Bundle>(filters).await?;
        tracing::Span::current().record("count", result.records.len());
        tracing::Span::current().record("corruption_count", result.corruption.len());
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
            role = %role,
            kind = ?kind,
            op = "update",
        ),
        ret,
    )]
    pub async fn update(
        &self,
        bundle: Bundle,
        expected_updated_at: i64,
        role: Role,
        kind: TargetKind,
    ) -> Result<i64, StoreError> {
        let result = self.update_and_persist(bundle, expected_updated_at, role, kind).await;
        // Phase 5 (docs/design/2026-07-12-failure-paths-recovery-chain.md):
        // an OCC Stale refusal is an expected, recoverable race at this
        // seam, not a store failure - the caller already owns the severity
        // verdict (`discriminate_stale_bundle_write`, Phase 4, fails loud
        // only when a Stale is a genuine invariant violation). The prior
        // blanket `#[instrument(err)]` logged ERROR on every benign lost
        // race, which trains operators to ignore ERROR entirely. Everything
        // else (I/O, corruption, illegal FSM transition) is a real failure
        // and stays loud. Sibling of `WorksStore::update`'s identical split.
        match &result {
            Ok(_) => {}
            Err(StoreError::Stale { expected, actual }) => {
                warn!(
                    expected_updated_at = expected,
                    actual_updated_at = actual,
                    "bundles.update: OCC Stale refusal (benign race, caller decides severity)"
                );
            }
            Err(other) => {
                error!(error = %other, "bundles.update: update failed");
            }
        }
        result
    }

    /// Body of `update`, split out so the outer fn can discriminate the
    /// Stale-vs-other log level at one boundary (see `update` above).
    async fn update_and_persist(
        &self,
        mut bundle: Bundle,
        expected_updated_at: i64,
        role: Role,
        kind: TargetKind,
    ) -> Result<i64, StoreError> {
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
        // Phase 9 FSM chokepoint: re-validate the persisted status change
        // against the table the caller declared via `kind`. Same-status
        // writes (field-only updates, e.g. the Reviewer's `verification`
        // edit) bypass edge validation. Behavior-neutral for legal flows
        // (production callers transitioned via the FSM first); fails closed
        // on a direct-assignment path.
        if current.status != bundle.status {
            let validated = match kind {
                TargetKind::Normal => BundleStatus::validate_transition(current.status, bundle.status, role),
                TargetKind::Override => BundleStatus::validate_override(current.status, bundle.status, role),
            };
            if validated.is_err() {
                return Err(StoreError::IllegalTransition {
                    record_kind: "bundle",
                    from: current.status.to_string(),
                    to: bundle.status.to_string(),
                    role,
                });
            }
        }
        // Monotonic `updated_at` floor under the lock; see
        // `WorksStore::update` for the same-millisecond OCC/merge-driver
        // tie-break rationale and why the floored value is returned (the
        // Integrator chains transitions on one in-memory Bundle). Floor
        // only ever moves the timestamp forward.
        let floored = std::cmp::max(domain::now_millis(), current.updated_at + 1);
        bundle.updated_at = floored;
        self.inner.update(bundle).await?;
        Ok(floored)
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
/// the caller took before mutating its clone. On success returns the
/// persisted (monotonically-floored) `updated_at` so the Integrator,
/// which chains transitions on one in-memory Bundle, can refresh its
/// expected version between writes (see `BundlesStore::update`).
#[trait_variant::make(Send)]
pub trait BundleUpdateSink: Send + Sync {
    async fn update(
        &self,
        bundle: Bundle,
        expected_updated_at: i64,
        role: Role,
        kind: TargetKind,
    ) -> Result<i64, BundleUpdateError>;
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
    async fn update(
        &self,
        bundle: Bundle,
        expected_updated_at: i64,
        role: Role,
        kind: TargetKind,
    ) -> Result<i64, BundleUpdateError> {
        match self.bundles().update(bundle, expected_updated_at, role, kind).await {
            Ok(ts) => Ok(ts),
            Err(StoreError::Stale { expected, actual }) => Err(BundleUpdateError::Stale { expected, actual }),
            Err(other) => Err(BundleUpdateError::Update(other.to_string())),
        }
    }
}

/// Forwarding impl for any reference to a `BundleUpdateSink`. Lets
/// callers build deps with a borrowed `Store` without cloning.
impl<B: BundleUpdateSink + ?Sized> BundleUpdateSink for &B {
    async fn update(
        &self,
        bundle: Bundle,
        expected_updated_at: i64,
        role: Role,
        kind: TargetKind,
    ) -> Result<i64, BundleUpdateError> {
        (*self).update(bundle, expected_updated_at, role, kind).await
    }
}

/// Forwarding impl for `Arc<B>`.
impl<B: BundleUpdateSink + ?Sized> BundleUpdateSink for std::sync::Arc<B> {
    async fn update(
        &self,
        bundle: Bundle,
        expected_updated_at: i64,
        role: Role,
        kind: TargetKind,
    ) -> Result<i64, BundleUpdateError> {
        (**self).update(bundle, expected_updated_at, role, kind).await
    }
}

#[cfg(test)]
mod tests;
