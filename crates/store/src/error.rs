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
}

impl From<taskstore_async::Error> for StoreError {
    fn from(e: taskstore_async::Error) -> Self {
        match e {
            taskstore_async::Error::Serde(inner) => StoreError::Serde(inner.to_string()),
            other => StoreError::Io(other.to_string()),
        }
    }
}
