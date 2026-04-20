use std::path::Path;

use taskstore_async::{AsyncStore, OpenOptions};

use crate::error::StoreError;

pub struct Store {
    inner: AsyncStore,
}

impl Store {
    pub async fn open(target: impl AsRef<Path>) -> Result<Self, StoreError> {
        let inner = AsyncStore::open(target.as_ref(), OpenOptions::default()).await?;
        Ok(Self { inner })
    }

    pub async fn close(self) -> Result<(), StoreError> {
        self.inner.close().await?;
        Ok(())
    }
}
