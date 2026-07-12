//! Integration tests for `Store::open`'s `.version` schema-guard
//! (Phase 3 F13). taskstore writes `.version` write-if-absent but does
//! not re-validate it on open; `Store::open` reads it back and rejects a
//! mismatch.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tempfile::TempDir;

use store::{STORE_VERSION, Store, StoreError};

#[tokio::test]
async fn open_succeeds_when_version_matches() {
    let dir = TempDir::new().unwrap();
    // First open writes `.version` with the matching version.
    let store = Store::open(dir.path()).await.expect("first open");
    store.close().await.expect("close");
    // Re-open of a same-version store succeeds.
    let store = Store::open(dir.path()).await.expect("reopen same version");
    store.close().await.expect("close");

    let version_file = dir.path().join(".loopr/taskstore/.version");
    let raw = std::fs::read_to_string(&version_file).unwrap();
    assert_eq!(
        raw.trim(),
        STORE_VERSION.to_string(),
        "on-disk version matches the const"
    );
}

#[tokio::test]
async fn open_rejects_mismatched_version() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).await.expect("first open");
    store.close().await.expect("close");

    // Corrupt the version to an incompatible future value.
    let version_file = dir.path().join(".loopr/taskstore/.version");
    std::fs::write(&version_file, (STORE_VERSION + 1).to_string()).unwrap();

    match Store::open(dir.path()).await {
        Err(StoreError::VersionMismatch { found, expected }) => {
            assert_eq!(found, STORE_VERSION + 1);
            assert_eq!(expected, STORE_VERSION);
        }
        Ok(_) => panic!("mismatched version must reject"),
        Err(other) => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn open_rejects_unparseable_version() {
    // Phase 6 F6 (2026-07-11-verified-swarm): an unparseable `.version` used
    // to fold into `VersionMismatch { found: 0, .. }`, indistinguishable from
    // a real "on-disk version is 0" store. This inverts that pin: the
    // unparseable case now gets its own typed variant carrying the raw
    // on-disk string, so a hand-edited or corrupt `.version` is diagnosable
    // without guessing whether "0" was synthesized or genuine.
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).await.expect("first open");
    store.close().await.expect("close");

    let version_file = dir.path().join(".loopr/taskstore/.version");
    std::fs::write(&version_file, "not-a-number").unwrap();

    match Store::open(dir.path()).await {
        Err(StoreError::UnparseableVersion { raw }) => {
            assert_eq!(raw, "not-a-number");
        }
        Ok(_) => panic!("unparseable version must reject"),
        Err(other) => panic!("expected UnparseableVersion, got {other:?}"),
    }
}

/// Phase 6 F6: `Store::open`'s version-check error paths must `close()`
/// the just-opened `AsyncStore` before returning `Err`, instead of
/// letting it drop implicitly. An implicit drop tears down the writer
/// thread via `AsyncStore`'s synchronous `Drop` impl (a blocking
/// `JoinHandle::join()` on whichever thread is unwinding the `Err`
/// return — here, a tokio worker), rather than the async-safe `close()`
/// path. Break-to-prove shape: bound the failing open in a tight
/// `tokio::time::timeout` on the single-threaded `#[tokio::test]`
/// (current_thread) runtime, where a blocking join on that lone thread
/// would be maximally visible, then prove the store was left in a
/// clean, reusable state by fixing the version file and
/// reopening/closing it, also under a tight timeout. Neither step may
/// hang the reactor.
#[tokio::test]
async fn open_error_paths_close_store_and_do_not_stall_reactor() {
    const BOUND: Duration = Duration::from_secs(5);

    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).await.expect("first open");
    store.close().await.expect("close");

    let version_file = dir.path().join(".loopr/taskstore/.version");
    std::fs::write(&version_file, (STORE_VERSION + 1).to_string()).unwrap();

    // The mismatched-version open must return promptly, not hang the
    // single-worker-thread reactor while tearing down the writer thread.
    let mismatched = tokio::time::timeout(BOUND, Store::open(dir.path()))
        .await
        .expect("mismatched-version open must not stall the reactor");
    assert!(matches!(mismatched, Err(StoreError::VersionMismatch { .. })));

    // Correct the version file and reopen. If the prior error path had
    // leaked or wedged the store (leaked writer thread, held lock), this
    // reopen would hang or fail; it must succeed promptly instead.
    std::fs::write(&version_file, STORE_VERSION.to_string()).unwrap();
    let reopened = tokio::time::timeout(BOUND, Store::open(dir.path()))
        .await
        .expect("reopen after a closed error path must not stall the reactor")
        .expect("reopen after a closed error path must succeed");
    tokio::time::timeout(BOUND, reopened.close())
        .await
        .expect("close after reopen must not stall the reactor")
        .expect("close after reopen must succeed");
}
