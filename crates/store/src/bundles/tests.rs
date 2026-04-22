//! Tests for `BundlesStore` — round-trip CRUD and OCC update
//! behaviour.
//!
//! The OCC tests are the interesting ones: they pin the intra-daemon
//! race-defense invariant. See
//! `docs/design/2026-04-22-reviewer.md` -> "Intra-daemon OCC via
//! Mutex + updated_at version check" for the rationale.

use std::sync::Arc;

use domain::{Bundle, BundleStatus, Role, WorkId};
use tempfile::TempDir;

use crate::{Store, StoreError};

async fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    (dir, store)
}

fn fresh_bundle() -> Bundle {
    Bundle::new(
        WorkId::new(),
        "loopr/test-branch".to_string(),
        vec!["test claim".to_string()],
    )
}

#[tokio::test]
async fn update_round_trip_ok() {
    let (_dir, store) = open_store().await;
    let bundle = fresh_bundle();
    let id = store.bundles().create(bundle.clone()).await.expect("create");

    let stored = store.bundles().get(&id).await.expect("get after create");
    let expected_updated_at = stored.updated_at;

    let mut next = stored.clone();
    next.transition(BundleStatus::Triaged, Role::Coordinator)
        .expect("triage");
    next.transition(BundleStatus::Reviewed, Role::Reviewer).expect("review");
    next.verification = "Reviewer approved: test".to_string();

    store
        .bundles()
        .update(next.clone(), expected_updated_at)
        .await
        .expect("update");

    let after = store.bundles().get(&id).await.expect("get after update");
    assert_eq!(after.status, BundleStatus::Reviewed);
    assert_eq!(after.verification, "Reviewer approved: test");
    assert!(after.updated_at > expected_updated_at, "updated_at should advance");
}

#[tokio::test]
async fn update_stale_version_rejected() {
    let (_dir, store) = open_store().await;
    let bundle = fresh_bundle();
    let id = store.bundles().create(bundle.clone()).await.expect("create");

    let stored = store.bundles().get(&id).await.expect("get");
    let snapshot = stored.updated_at;

    let mut first = stored.clone();
    first
        .transition(BundleStatus::Triaged, Role::Coordinator)
        .expect("triage");
    first
        .transition(BundleStatus::Reviewed, Role::Reviewer)
        .expect("review");
    first.verification = "first winner".to_string();
    store
        .bundles()
        .update(first.clone(), snapshot)
        .await
        .expect("first update");

    let mut second = stored;
    second
        .transition(BundleStatus::Triaged, Role::Coordinator)
        .expect("triage");
    second
        .transition(BundleStatus::Rejected, Role::Reviewer)
        .expect("reject");
    second.verification = "second would overwrite".to_string();
    let err = store.bundles().update(second, snapshot).await.unwrap_err();
    match err {
        StoreError::Stale { expected, actual } => {
            assert_eq!(expected, snapshot);
            assert!(actual > expected);
        }
        other => panic!("expected Stale, got {other:?}"),
    }

    let after = store.bundles().get(&id).await.expect("get");
    assert_eq!(after.verification, "first winner");
    assert_eq!(after.status, BundleStatus::Reviewed);
}

#[tokio::test]
async fn update_unknown_id_yields_not_found() {
    let (_dir, store) = open_store().await;
    let bundle = fresh_bundle();
    let err = store.bundles().update(bundle, 0).await.unwrap_err();
    match err {
        StoreError::RecordNotFound { collection, .. } => {
            assert_eq!(collection, "bundles");
        }
        other => panic!("expected RecordNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_updates_produce_exactly_one_winner() {
    let (_dir, store) = open_store().await;
    let bundle = fresh_bundle();
    let id = store.bundles().create(bundle.clone()).await.expect("create");

    let stored = store.bundles().get(&id).await.expect("get");
    let snapshot = stored.updated_at;

    let store = Arc::new(store);

    let s1 = Arc::clone(&store);
    let mut b1 = stored.clone();
    b1.transition(BundleStatus::Triaged, Role::Coordinator).expect("triage");
    b1.transition(BundleStatus::Reviewed, Role::Reviewer).expect("review");
    b1.verification = "task A".to_string();

    let s2 = Arc::clone(&store);
    let mut b2 = stored.clone();
    b2.transition(BundleStatus::Triaged, Role::Coordinator).expect("triage");
    b2.transition(BundleStatus::Rejected, Role::Reviewer).expect("reject");
    b2.verification = "task B".to_string();

    let h1 = tokio::spawn(async move { s1.bundles().update(b1, snapshot).await });
    let h2 = tokio::spawn(async move { s2.bundles().update(b2, snapshot).await });

    let r1 = h1.await.expect("join");
    let r2 = h2.await.expect("join");

    let oks = [r1.is_ok(), r2.is_ok()].into_iter().filter(|b| *b).count();
    let stales = [&r1, &r2]
        .into_iter()
        .filter(|r| matches!(r, Err(StoreError::Stale { .. })))
        .count();
    assert_eq!(oks, 1, "expected exactly one success, r1={r1:?} r2={r2:?}");
    assert_eq!(stales, 1, "expected exactly one stale, r1={r1:?} r2={r2:?}");

    let after = store.bundles().get(&id).await.expect("get");
    assert!(
        after.verification == "task A" || after.verification == "task B",
        "verification = {}",
        after.verification
    );
}
