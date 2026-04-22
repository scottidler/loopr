use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error in taskstore: {0}")]
    Io(String),

    #[error("record not found: {collection}/{id}")]
    RecordNotFound { collection: &'static str, id: String },

    #[error("record already exists: {collection}/{id}")]
    AlreadyExists { collection: &'static str, id: String },

    #[allow(dead_code)]
    #[error("corrupt record in taskstore: {0}")]
    Corruption(String),

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
}

impl From<taskstore_async::Error> for StoreError {
    fn from(e: taskstore_async::Error) -> Self {
        match e {
            taskstore_async::Error::Serde(inner) => StoreError::Serde(inner.to_string()),
            other => StoreError::Io(other.to_string()),
        }
    }
}
