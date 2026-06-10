use domain::{BundleId, PlanId, TickId};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error in taskstore: {0}")]
    Io(String),

    #[error("record not found: {collection}/{id}")]
    RecordNotFound { collection: &'static str, id: String },

    #[error("record already exists: {collection}/{id}")]
    AlreadyExists { collection: &'static str, id: String },

    /// The underlying store's writer-thread channel is closed — the
    /// benign graceful-shutdown race where a write loses to `close()`.
    /// Kept distinct from `Io` so shutdown paths can special-case it
    /// (a write that lost the shutdown race is not a hard I/O failure).
    #[error("store closed (shutting down)")]
    Closed,

    /// On-disk `.version` does not match the schema version this build
    /// expects (`store::STORE_VERSION`). Raised by `Store::open` after
    /// the taskstore write-if-absent step, so an incompatible store
    /// surfaces as an explicit open-time error rather than mismatched
    /// reads.
    #[error("store version mismatch: on-disk={found}, expected={expected}")]
    VersionMismatch { found: u32, expected: u32 },

    #[error("serde failure at store boundary: {0}")]
    Serde(String),

    /// OCC version-check failure. Raised by `BundlesStore::update` when
    /// the Bundle on disk has a newer `updated_at` than the version the
    /// caller snapshotted before mutating. The caller is expected to
    /// re-fetch and decide whether to retry or drop the write.
    ///
    /// Note on naming: the Reviewer design doc (2026-04-22) sketched a
    /// sibling `NotFound(String)` variant here; the existing
    /// `RecordNotFound { collection, id }` covers that case with
    /// strictly more information, so no `NotFound` was added.
    #[error("stale record: expected updated_at={expected}, actual={actual}")]
    Stale { expected: i64, actual: i64 },

    /// `TicksStore::create` uniqueness failure. Raised when an incoming
    /// Tick's `(plan_id, bundles-as-set)` matches a Tick already
    /// persisted for that Plan. Carries `tick_id` so the Integrator's
    /// crash-recovery path can resolve the existing Tick with one
    /// `TicksStore::get(tick_id)` rather than a second
    /// `list_by_plan_id` scan.
    #[error("duplicate tick: existing tick_id={tick_id} for plan_id={plan_id} with bundles={bundles:?}")]
    DuplicateTick {
        tick_id: TickId,
        plan_id: PlanId,
        bundles: Vec<BundleId>,
    },
}

// `taskstore_async::Error` is a re-export of `taskstore::Error` (the
// workspace unified its error type at v0.6.0). Naming the re-exported
// path here avoids dragging the `taskstore` crate into our direct deps.
impl From<taskstore_async::Error> for StoreError {
    fn from(e: taskstore_async::Error) -> Self {
        match e {
            taskstore_async::Error::Serde(inner) => StoreError::Serde(inner.to_string()),
            // Benign shutdown race: the writer channel closed. Special-cased
            // so daemon shutdown paths don't treat it as a hard I/O failure.
            taskstore_async::Error::StoreClosed => StoreError::Closed,
            other => StoreError::Io(other.to_string()),
        }
    }
}
