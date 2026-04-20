use std::str::FromStr;

use tempfile::TempDir;

use domain::{Plan, PlanId, PlanStatus};
use store::{PlansStore, Store, StoreError};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn plans_store_is_send_sync() {
    assert_send_sync::<Store>();
    assert_send_sync::<PlansStore<'static>>();
}

async fn fresh_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    (dir, store)
}

#[tokio::test]
async fn close_returns_ok_on_fresh_store() {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    store.close().await.expect("close");
}

#[tokio::test]
async fn empty_store_list_returns_empty_vec() {
    let (_dir, store) = fresh_store().await;
    let plans = store.plans().list().await.expect("list");
    assert!(plans.is_empty());
}

#[tokio::test]
async fn create_returns_id_that_resolves() {
    let (_dir, store) = fresh_store().await;
    let plan = Plan::new("test goal".to_string());
    let id = store.plans().create(plan.clone()).await.expect("create");
    let got = store.plans().get(&id).await.expect("get");
    assert_eq!(got.id, plan.id);
    assert_eq!(got.goal, plan.goal);
}

#[tokio::test]
async fn create_then_get_roundtrip() {
    let (_dir, store) = fresh_store().await;
    let plan = Plan::new("roundtrip".to_string());
    let id = store.plans().create(plan.clone()).await.expect("create");
    let got = store.plans().get(&id).await.expect("get");
    assert_eq!(got.id, plan.id);
    assert_eq!(got.updated_at, plan.updated_at);
    assert_eq!(got.created_at, plan.created_at);
    assert_eq!(got.goal, plan.goal);
    assert_eq!(got.status, plan.status);
}

#[tokio::test]
async fn create_then_list_sees_record() {
    let (_dir, store) = fresh_store().await;
    let plan = Plan::new("seen in list".to_string());
    let id = store.plans().create(plan.clone()).await.expect("create");
    let listed = store.plans().list().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].goal, plan.goal);
}

#[tokio::test]
async fn create_two_then_list_returns_both() {
    let (_dir, store) = fresh_store().await;
    let p1 = Plan::new("first".to_string());
    let p2 = Plan::new("second".to_string());
    let id1 = store.plans().create(p1).await.expect("create p1");
    let id2 = store.plans().create(p2).await.expect("create p2");
    let listed = store.plans().list().await.expect("list");
    assert_eq!(listed.len(), 2);
    let ids: Vec<_> = listed.iter().map(|p| p.id.clone()).collect();
    assert!(ids.contains(&id1), "id1 should be in list");
    assert!(ids.contains(&id2), "id2 should be in list");
}

#[tokio::test]
async fn get_missing_returns_record_not_found() {
    let (_dir, store) = fresh_store().await;
    let nonexistent = PlanId::from_str("pl-xxxxx").expect("infallible");
    let err = store.plans().get(&nonexistent).await.expect_err("should miss");
    match err {
        StoreError::RecordNotFound { collection, id } => {
            assert_eq!(collection, "plans");
            assert_eq!(id, "pl-xxxxx");
        }
        other => panic!("expected RecordNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn create_same_id_twice_returns_already_exists() {
    let (_dir, store) = fresh_store().await;
    let p1 = Plan::new("first".to_string());
    let p2 = Plan {
        id: p1.id.clone(),
        updated_at: p1.updated_at,
        created_at: p1.created_at,
        goal: "second with same id".to_string(),
        status: p1.status,
    };
    store.plans().create(p1).await.expect("first create");
    let err = store.plans().create(p2).await.expect_err("second should reject");
    match err {
        StoreError::AlreadyExists { collection, id: _ } => {
            assert_eq!(collection, "plans");
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    let listed = store.plans().list().await.expect("list");
    assert_eq!(listed.len(), 1, "duplicate should not have overwritten");
}

#[tokio::test]
async fn serde_roundtrip_survives_jsonl() {
    let (_dir, store) = fresh_store().await;

    let cases = [
        ("unicode: 日本語 émojis 🌊", PlanStatus::Active),
        ("", PlanStatus::Draft),
        ("pending", PlanStatus::Pending),
        ("done", PlanStatus::Complete),
        ("replaced", PlanStatus::Superseded),
        ("given up", PlanStatus::Abandoned),
    ];

    for (goal, status) in cases.iter() {
        let mut plan = Plan::new(goal.to_string());
        plan.status = *status;
        let id = store.plans().create(plan.clone()).await.expect("create");
        let got = store.plans().get(&id).await.expect("get");
        assert_eq!(got.goal, plan.goal, "goal should roundtrip for status {status:?}");
        assert_eq!(got.status, plan.status, "status should roundtrip for {goal}");
    }

    let listed = store.plans().list().await.expect("list");
    assert_eq!(listed.len(), cases.len());
}
