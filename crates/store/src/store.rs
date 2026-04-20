use std::path::Path;

use taskstore_async::{AsyncStore, OpenOptions};

use crate::error::StoreError;
use crate::plans::PlansStore;

pub struct Store {
    inner: AsyncStore,
}

impl Store {
    /// Open the store rooted at the given target directory.
    ///
    /// `taskstore_async::AsyncStore::open` appends `.taskstore/` to its
    /// argument and creates the subdirectory on first call. Pass the target
    /// repo root; do NOT pre-append `.taskstore/` or the subdirectory gets
    /// nested.
    pub async fn open(target: impl AsRef<Path>) -> Result<Self, StoreError> {
        let inner = AsyncStore::open(target.as_ref(), OpenOptions::default()).await?;
        Ok(Self { inner })
    }

    /// Typed accessor for Plan records. Borrowed, zero-cost handle.
    pub fn plans(&self) -> PlansStore<'_> {
        PlansStore::new(&self.inner)
    }

    /// Graceful async shutdown. Drops the writer queue, awaits the writer
    /// thread's drain signal, joins cleanly. Consumes `self` so the wrapper
    /// cannot be used after close.
    ///
    /// **Must be called before Drop on a tokio runtime.** `AsyncStore::Drop`
    /// joins the writer thread synchronously; if `Store` is dropped on a tokio
    /// executor thread (runtime shutdown, task panic, daemon exit), the sync
    /// join blocks the reactor and can trigger tokio's "Cannot block the
    /// current thread" panic — exactly when flush-to-disk needs to succeed
    /// most. Daemon shutdown handlers MUST invoke `close().await` before the
    /// runtime terminates; `Drop` is a last-resort fallback for crash-interrupt
    /// paths only.
    pub async fn close(self) -> Result<(), StoreError> {
        self.inner.close().await?;
        Ok(())
    }
}
