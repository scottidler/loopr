use std::path::Path;

use taskstore_async::{AsyncStore, OpenOptions};
use tokio::sync::Mutex;
use tracing::instrument;

use crate::bundles::BundlesStore;
use crate::error::StoreError;
use crate::notes::NotesStore;
use crate::plans::PlansStore;
use crate::ticks::TicksStore;
use crate::works::WorksStore;

/// Path of the taskstore directory relative to the target repo root.
/// v5 nests taskstore beneath the transient `.loopr/` directory so a
/// target's entire loopr footprint (logs, pid, socket, store) lives
/// under a single top-level folder. The path is created on first open
/// via `AsyncStore::open_at`.
pub const TASKSTORE_SUBPATH: &str = ".loopr/taskstore";

/// Schema version this build expects in the store's `.version` file.
/// taskstore writes `.version` write-if-absent on `open_at` (its
/// internal `CURRENT_VERSION`); it does NOT re-validate on subsequent
/// opens. `Store::open` reads it back and compares against this const so
/// a store written by an incompatible taskstore (or hand-edited) fails
/// open explicitly rather than being read with mismatched assumptions.
/// Bump in lockstep with taskstore's `CURRENT_VERSION`.
pub const STORE_VERSION: u32 = 1;

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
    /// Intra-daemon OCC serializer for `WorksStore::update`. Mirrors
    /// `bundle_update_lock` exactly. Guards the concurrent-promotion
    /// race where two sibling Works go Done simultaneously and both
    /// `promote_unblocked_siblings` calls find the same Pending Work
    /// eligible, preventing double Implementer spawning.
    work_update_lock: Mutex<()>,
    /// Intra-daemon serializer for `TicksStore::create`'s duplicate-
    /// detection read-check-write. Without it, two concurrent
    /// `integrate` calls in the crash-recovery path could both see
    /// empty `list_by_plan_id` and both append, producing two Ticks
    /// for one merge. Cross-process protection is not in scope
    /// (single-daemon-per-target by `.loopr/daemon.pid`).
    ///
    /// Same placement rationale as `bundle_update_lock`: the lock
    /// lives on `Store`, not inside the `TicksStore<'a>` handle,
    /// because `.ticks()` returns a fresh handle on each call.
    tick_lock: Mutex<()>,
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
    #[instrument(name = "store.open", level = "info", skip_all, fields(target = %target.as_ref().display()), err)]
    pub async fn open(target: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = target.as_ref().join(TASKSTORE_SUBPATH);
        let inner = AsyncStore::open_at(&path, OpenOptions::default()).await?;
        // Validate the on-disk schema version. `open_at` has already
        // written `.version` write-if-absent, so on a fresh store the
        // file now holds taskstore's `CURRENT_VERSION`; on a pre-existing
        // store it holds whatever wrote it. A mismatch (or an
        // unparseable file) is an explicit open-time error.
        // One-shot read at open; `std::fs` (not `tokio::fs`, which this
        // crate doesn't enable) is fine for a tiny file read once per
        // process lifetime.
        let version_path = path.join(".version");
        match std::fs::read_to_string(&version_path) {
            Ok(raw) => {
                let found: u32 = raw.trim().parse().map_err(|_| StoreError::VersionMismatch {
                    found: 0,
                    expected: STORE_VERSION,
                })?;
                if found != STORE_VERSION {
                    return Err(StoreError::VersionMismatch {
                        found,
                        expected: STORE_VERSION,
                    });
                }
            }
            // `open_at` guarantees the file exists post-open; a missing
            // file here is unexpected but not corrupting — taskstore owns
            // write-if-absent, so treat absence as "fresh, trust the open."
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::Io(format!("reading {}: {e}", version_path.display()))),
        }
        Ok(Self {
            inner,
            bundle_update_lock: Mutex::new(()),
            work_update_lock: Mutex::new(()),
            tick_lock: Mutex::new(()),
        })
    }

    /// Typed accessor for Plan records. Borrowed, zero-cost handle.
    pub fn plans(&self) -> PlansStore<'_> {
        PlansStore::new(&self.inner)
    }

    /// Typed accessor for Work records. Borrowed, zero-cost handle.
    /// The handle borrows the parent `Store`'s OCC mutex so
    /// `update(work, expected_updated_at)` calls serialize across
    /// all callers of the same `Store`, not just within one handle.
    pub fn works(&self) -> WorksStore<'_> {
        WorksStore::new(&self.inner, &self.work_update_lock)
    }

    /// Typed accessor for Bundle records. Borrowed, zero-cost handle.
    /// The handle borrows the parent `Store`'s OCC mutex so
    /// `update(bundle, expected_updated_at)` calls serialize across
    /// all callers of the same `Store`, not just within one handle.
    pub fn bundles(&self) -> BundlesStore<'_> {
        BundlesStore::new(&self.inner, &self.bundle_update_lock)
    }

    /// Typed accessor for Tick records. Borrowed, zero-cost handle.
    /// The handle borrows the parent `Store`'s tick lock so
    /// `create(tick)` duplicate-detection serializes across all
    /// callers of the same `Store`.
    pub fn ticks(&self) -> TicksStore<'_> {
        TicksStore::new(&self.inner, &self.tick_lock)
    }

    /// Typed accessor for OperatorNote records. Borrowed, zero-cost
    /// handle. No write lock: notes have one writer per role (IPC
    /// handler creates, Director task marks read), and `read_at` is a
    /// monotonic `None -> Some` transition.
    pub fn notes(&self) -> NotesStore<'_> {
        NotesStore::new(&self.inner)
    }

    /// Install taskstore's git hooks under `.git/hooks/`. Idempotent;
    /// safe to call on every boot. Phase 9 of the Tier-1 cleanup
    /// surfaces this through `loopr init`'s six-step orchestrator so
    /// the merge-driver hooks are present on every new target.
    #[instrument(name = "store.install_git_hooks", level = "info", skip_all, err)]
    pub async fn install_git_hooks(&self) -> Result<(), StoreError> {
        self.inner.install_git_hooks().await?;
        Ok(())
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
    #[instrument(name = "store.close", level = "info", skip_all, err)]
    pub async fn close(self) -> Result<(), StoreError> {
        self.inner.close().await?;
        Ok(())
    }
}
