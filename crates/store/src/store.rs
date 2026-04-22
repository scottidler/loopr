use std::path::Path;

use taskstore_async::{AsyncStore, OpenOptions};
use tokio::sync::Mutex;

use crate::bundles::BundlesStore;
use crate::error::StoreError;
use crate::plans::PlansStore;
use crate::works::WorksStore;

/// Path of the taskstore directory relative to the target repo root.
/// v5 nests taskstore beneath the transient `.loopr/` directory so a
/// target's entire loopr footprint (logs, pid, socket, store) lives
/// under a single top-level folder. The path is created on first open
/// via `AsyncStore::open_at`.
pub const TASKSTORE_SUBPATH: &str = ".loopr/taskstore";

pub struct Store {
    inner: AsyncStore,
    /// Intra-daemon OCC serializer for `BundlesStore::update`. The
    /// Bundle update path is (read-current, compare-updated_at,
    /// write); the daemon can have multiple Reviewer tasks racing on
    /// the same Bundle (Stage 8 wiring), so the read-check-write must
    /// be atomic under one daemon. Cross-process protection is not in
    /// scope (single-daemon-per-target by `.loopr/daemon.pid`).
    ///
    /// The lock lives on `Store` (not inside `BundlesStore<'a>`)
    /// because `.bundles()` returns a fresh handle on each call; a
    /// Mutex local to the handle would serialize nothing between two
    /// `.bundles().update(...)` calls.
    bundle_update_lock: Mutex<()>,
}

impl Store {
    /// Open the store rooted at the given target directory. The on-disk
    /// location is `<target>/.loopr/taskstore/` (see `TASKSTORE_SUBPATH`).
    ///
    /// Upstream note: as of `taskstore 0.5.0` / `taskstore-async 0.2.0`
    /// the library no longer appends any path segment of its own;
    /// callers pick the exact directory via `open_at`. We use that here
    /// to keep loopr's full state under a single `.loopr/` top-level
    /// folder. Passing a bare `target` to the old `open(path, opts)` is
    /// an API that no longer exists; this wrapper is the sole consumer
    /// that needs to know about the nested layout.
    pub async fn open(target: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = target.as_ref().join(TASKSTORE_SUBPATH);
        let inner = AsyncStore::open_at(path, OpenOptions::default()).await?;
        Ok(Self {
            inner,
            bundle_update_lock: Mutex::new(()),
        })
    }

    /// Typed accessor for Plan records. Borrowed, zero-cost handle.
    pub fn plans(&self) -> PlansStore<'_> {
        PlansStore::new(&self.inner)
    }

    /// Typed accessor for Work records. Borrowed, zero-cost handle.
    pub fn works(&self) -> WorksStore<'_> {
        WorksStore::new(&self.inner)
    }

    /// Typed accessor for Bundle records. Borrowed, zero-cost handle.
    /// The handle borrows the parent `Store`'s OCC mutex so
    /// `update(bundle, expected_updated_at)` calls serialize across
    /// all callers of the same `Store`, not just within one handle.
    pub fn bundles(&self) -> BundlesStore<'_> {
        BundlesStore::new(&self.inner, &self.bundle_update_lock)
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
