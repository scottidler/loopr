//! Tests for `TicksStore`: CRUD + duplicate-detection behaviour under
//! `tick_lock`. The duplicate-detection tests pin the invariant the
//! Integrator's crash-recovery path depends on
//! (`StoreError::DuplicateTick` carries the existing Tick's id).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use domain::{BundleId, PlanId, Tick};
use tempfile::TempDir;

use crate::{Store, StoreError};

async fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    (dir, store)
}

fn fresh_tick(plan_id: PlanId, bundles: Vec<BundleId>) -> Tick {
    Tick::new(
        plan_id,
        bundles,
        "loopr/plan-xxx".to_string(),
        "abc123".to_string(),
        vec!["def456".to_string()],
    )
}

#[tokio::test]
async fn create_then_get_round_trip() {
    let (_dir, store) = open_store().await;
    let pid = PlanId::new();
    let bid = BundleId::new();
    let tick = fresh_tick(pid.clone(), vec![bid.clone()]);
    let tick_id = tick.id.clone();

    let returned = store.ticks().create(tick.clone()).await.expect("create");
    assert_eq!(returned.id, tick_id);
    assert_eq!(returned.plan_id, pid);

    let fetched = store.ticks().get(&tick_id).await.expect("get");
    assert_eq!(fetched.id, tick_id);
    assert_eq!(fetched.bundles, vec![bid]);
}

#[tokio::test]
async fn get_missing_returns_record_not_found() {
    let (_dir, store) = open_store().await;
    let result = store.ticks().get(&domain::TickId::new()).await;
    assert!(matches!(
        result,
        Err(StoreError::RecordNotFound {
            collection: "ticks",
            ..
        })
    ));
}

#[tokio::test]
async fn list_by_plan_id_returns_matching() {
    let (_dir, store) = open_store().await;
    let pid_a = PlanId::new();
    let pid_b = PlanId::new();

    let a1 = fresh_tick(pid_a.clone(), vec![BundleId::new()]);
    let a2 = fresh_tick(pid_a.clone(), vec![BundleId::new()]);
    let b1 = fresh_tick(pid_b.clone(), vec![BundleId::new()]);

    store.ticks().create(a1).await.expect("a1");
    store.ticks().create(a2).await.expect("a2");
    store.ticks().create(b1).await.expect("b1");

    let listed = store.ticks().list_by_plan_id(&pid_a).await.expect("list");
    assert_eq!(listed.len(), 2, "expected 2 Ticks for pid_a, got {}", listed.len());
    assert!(listed.iter().all(|t| t.plan_id == pid_a));
}

#[tokio::test]
async fn list_by_plan_id_empty_for_unknown() {
    let (_dir, store) = open_store().await;
    let listed = store.ticks().list_by_plan_id(&PlanId::new()).await.expect("list");
    assert!(listed.is_empty());
}

#[tokio::test]
async fn duplicate_tick_detected_same_bundle_set() {
    let (_dir, store) = open_store().await;
    let pid = PlanId::new();
    let bid = BundleId::new();

    let first = fresh_tick(pid.clone(), vec![bid.clone()]);
    let first_id = first.id.clone();
    store.ticks().create(first).await.expect("first create");

    // Same plan + same bundle set -> duplicate.
    let second = fresh_tick(pid.clone(), vec![bid.clone()]);
    let result = store.ticks().create(second).await;
    match result {
        Err(StoreError::DuplicateTick {
            tick_id,
            plan_id,
            bundles,
        }) => {
            assert_eq!(tick_id, first_id, "DuplicateTick must carry the existing tick's id");
            assert_eq!(plan_id, pid);
            assert_eq!(bundles, vec![bid]);
        }
        other => panic!("expected DuplicateTick, got: {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_detection_is_set_based_not_order_based() {
    // Reordering the bundles Vec must NOT produce a false non-duplicate:
    // the duplicate check compares the bundles-as-set, not the Vec order.
    let (_dir, store) = open_store().await;
    let pid = PlanId::new();
    let b1 = BundleId::new();
    let b2 = BundleId::new();

    let first = fresh_tick(pid.clone(), vec![b1.clone(), b2.clone()]);
    store.ticks().create(first).await.expect("first");

    let second = fresh_tick(pid.clone(), vec![b2.clone(), b1.clone()]);
    let result = store.ticks().create(second).await;
    assert!(
        matches!(result, Err(StoreError::DuplicateTick { .. })),
        "reordered bundle Vec must still be detected as duplicate"
    );
}

#[tokio::test]
async fn different_bundle_sets_allowed_on_same_plan() {
    let (_dir, store) = open_store().await;
    let pid = PlanId::new();

    let t1 = fresh_tick(pid.clone(), vec![BundleId::new()]);
    let t2 = fresh_tick(pid.clone(), vec![BundleId::new()]); // different bundle

    store.ticks().create(t1).await.expect("t1");
    store
        .ticks()
        .create(t2)
        .await
        .expect("t2 - different bundle set must succeed");
}

#[tokio::test]
async fn concurrent_create_same_bundles_exactly_one_wins() {
    // Two concurrent create calls on the same (plan, bundles). Without
    // `tick_lock`, both could pass `list_by_plan_id` returning empty and
    // both append. With the lock: first winner appends, second winner
    // sees the existing Tick and returns DuplicateTick.
    let (_dir, store) = open_store().await;
    let store = Arc::new(store);
    let pid = PlanId::new();
    let bid = BundleId::new();

    let t1 = fresh_tick(pid.clone(), vec![bid.clone()]);
    let t2 = fresh_tick(pid.clone(), vec![bid.clone()]);

    let s1 = store.clone();
    let s2 = store.clone();
    let (r1, r2) = tokio::join!(async move { s1.ticks().create(t1).await }, async move {
        s2.ticks().create(t2).await
    },);

    let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    let dups = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, Err(StoreError::DuplicateTick { .. })))
        .count();
    assert_eq!(oks, 1, "exactly one create must succeed: r1={r1:?} r2={r2:?}");
    assert_eq!(dups, 1, "exactly one create must return DuplicateTick");
}
