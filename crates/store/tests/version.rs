//! Integration tests for `Store::open`'s `.version` schema-guard
//! (Phase 3 F13). taskstore writes `.version` write-if-absent but does
//! not re-validate it on open; `Store::open` reads it back and rejects a
//! mismatch.

#![allow(clippy::unwrap_used)]

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
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).await.expect("first open");
    store.close().await.expect("close");

    let version_file = dir.path().join(".loopr/taskstore/.version");
    std::fs::write(&version_file, "not-a-number").unwrap();

    match Store::open(dir.path()).await {
        Err(StoreError::VersionMismatch { .. }) => {}
        Ok(_) => panic!("unparseable version must reject"),
        Err(other) => panic!("expected VersionMismatch, got {other:?}"),
    }
}
