//! Tests for `BundlesStore` — round-trip CRUD and OCC update
//! behaviour.
//!
//! The OCC tests are the interesting ones: they pin the intra-daemon
//! race-defense invariant. See
//! `docs/design/2026-04-22-reviewer.md` -> "Intra-daemon OCC via
//! Mutex + updated_at version check" for the rationale.

use std::sync::Arc;

use domain::{Bundle, BundleId, BundleStatus, Role, TargetKind, WorkId};
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

/// Create a Bundle and advance it to `Triaged` on disk (a single legal
/// `Proposed -> Triaged` Reactor edge), mirroring the production daemon
/// which triages before the Reviewer runs. Returns the id plus the
/// in-memory Triaged clone with its persisted `updated_at`. The OCC tests
/// below start from this realistic on-disk status so the persisted edge is
/// a single legal FSM hop (`Triaged -> Reviewed`/`Rejected`), which the
/// Phase 9 store chokepoint validates.
async fn create_triaged(store: &Store) -> (BundleId, Bundle) {
    let bundle = fresh_bundle();
    let id = store.bundles().create(bundle.clone()).await.expect("create");
    let stored = store.bundles().get(&id).await.expect("get after create");
    let mut triaged = stored.clone();
    triaged
        .transition(BundleStatus::Triaged, Role::Reactor)
        .expect("triage");
    let ts = store
        .bundles()
        .update(triaged.clone(), stored.updated_at, Role::Reactor, TargetKind::Normal)
        .await
        .expect("triage persist");
    triaged.updated_at = ts;
    (id, triaged)
}

#[tokio::test]
async fn update_round_trip_ok() {
    let (_dir, store) = open_store().await;
    let (id, triaged) = create_triaged(&store).await;
    let expected_updated_at = triaged.updated_at;

    let mut next = triaged.clone();
    next.transition(BundleStatus::Reviewed, Role::Reviewer).expect("review");
    next.verification = "Reviewer approved: test".to_string();

    store
        .bundles()
        .update(next.clone(), expected_updated_at, Role::Reviewer, TargetKind::Normal)
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
    let (id, triaged) = create_triaged(&store).await;
    let snapshot = triaged.updated_at;

    let mut first = triaged.clone();
    first
        .transition(BundleStatus::Reviewed, Role::Reviewer)
        .expect("review");
    first.verification = "first winner".to_string();
    store
        .bundles()
        .update(first.clone(), snapshot, Role::Reviewer, TargetKind::Normal)
        .await
        .expect("first update");

    let mut second = triaged;
    second
        .transition(BundleStatus::Rejected, Role::Reviewer)
        .expect("reject");
    second.verification = "second would overwrite".to_string();
    let err = store
        .bundles()
        .update(second, snapshot, Role::Reviewer, TargetKind::Normal)
        .await
        .unwrap_err();
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
    let err = store
        .bundles()
        .update(bundle, 0, Role::Reactor, TargetKind::Normal)
        .await
        .unwrap_err();
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
    let (id, triaged) = create_triaged(&store).await;
    let snapshot = triaged.updated_at;

    let store = Arc::new(store);

    let s1 = Arc::clone(&store);
    let mut b1 = triaged.clone();
    b1.transition(BundleStatus::Reviewed, Role::Reviewer).expect("review");
    b1.verification = "task A".to_string();

    let s2 = Arc::clone(&store);
    let mut b2 = triaged.clone();
    b2.transition(BundleStatus::Rejected, Role::Reviewer).expect("reject");
    b2.verification = "task B".to_string();

    let h1 = tokio::spawn(async move {
        s1.bundles()
            .update(b1, snapshot, Role::Reviewer, TargetKind::Normal)
            .await
    });
    let h2 = tokio::spawn(async move {
        s2.bundles()
            .update(b2, snapshot, Role::Reviewer, TargetKind::Normal)
            .await
    });

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
