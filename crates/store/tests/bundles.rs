use std::str::FromStr;

use tempfile::TempDir;

use domain::{Bundle, BundleId, Plan, Work, WorkId};
use store::{BundlesStore, Store, StoreError};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn bundles_store_is_send_sync() {
    assert_send_sync::<BundlesStore<'static>>();
}

async fn fresh_store_with_work() -> (TempDir, Store, Work) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    let plan = Plan::new("parent plan".to_string());
    store.plans().create(plan.clone()).await.expect("create plan");
    let work = Work::new(plan.id.clone(), "impl a thing".to_string());
    store.works().create(work.clone()).await.expect("create work");
    (dir, store, work)
}

fn sample_bundle(work: &Work, branch: &str) -> Bundle {
    Bundle::new(work.id.clone(), branch.to_string(), vec!["it compiles".to_string()])
}

#[tokio::test]
async fn empty_bundles_list_returns_empty_vec() {
    let (_dir, store, _work) = fresh_store_with_work().await;
    let bundles = store.bundles().list().await.expect("list");
    assert!(bundles.is_empty());
}

#[tokio::test]
async fn create_returns_id_that_resolves() {
    let (_dir, store, work) = fresh_store_with_work().await;
    let bundle = sample_bundle(&work, "wk-abc/1");
    let expected_id = bundle.id.clone();
    let id = store.bundles().create(bundle.clone()).await.expect("create");
    assert_eq!(id, expected_id);
    let got = store.bundles().get(&id).await.expect("get");
    assert_eq!(got.id, bundle.id);
    assert_eq!(got.work_id, work.id);
    assert_eq!(got.branch_name, "wk-abc/1");
}

#[tokio::test]
async fn create_then_list_sees_record() {
    let (_dir, store, work) = fresh_store_with_work().await;
    let bundle = sample_bundle(&work, "branch-a");
    store.bundles().create(bundle.clone()).await.expect("create");
    let listed = store.bundles().list().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].branch_name, "branch-a");
}

#[tokio::test]
async fn create_same_id_twice_returns_already_exists() {
    let (_dir, store, work) = fresh_store_with_work().await;
    let b1 = sample_bundle(&work, "one");
    let mut b2 = sample_bundle(&work, "two");
    b2.id = b1.id.clone();
    store.bundles().create(b1).await.expect("first create");
    let err = store.bundles().create(b2).await.expect_err("second should reject");
    match err {
        StoreError::AlreadyExists { collection, id: _ } => {
            assert_eq!(collection, "bundles");
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    let listed = store.bundles().list().await.expect("list");
    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn get_missing_returns_record_not_found() {
    let (_dir, store, _work) = fresh_store_with_work().await;
    let nonexistent = BundleId::from_str("bd-xxxxx").expect("infallible");
    let err = store.bundles().get(&nonexistent).await.expect_err("should miss");
    match err {
        StoreError::RecordNotFound { collection, id } => {
            assert_eq!(collection, "bundles");
            assert_eq!(id, "bd-xxxxx");
        }
        other => panic!("expected RecordNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn list_by_work_id_returns_only_matching() {
    let (_dir, store, work_a) = fresh_store_with_work().await;
    // Create a second Work under the same Plan.
    let plans = store.plans().list().await.expect("list plans");
    let work_b = Work::new(plans[0].id.clone(), "another thing".to_string());
    store.works().create(work_b.clone()).await.expect("create work b");

    let b1 = sample_bundle(&work_a, "a/1");
    let b2 = sample_bundle(&work_a, "a/2");
    let b3 = sample_bundle(&work_b, "b/1");
    store.bundles().create(b1.clone()).await.expect("b1");
    store.bundles().create(b2.clone()).await.expect("b2");
    store.bundles().create(b3.clone()).await.expect("b3");

    let for_a = store.bundles().list_by_work_id(&work_a.id).await.expect("list a");
    assert_eq!(for_a.len(), 2, "work_a should have 2 bundles");
    let branch_names_a: Vec<String> = for_a.iter().map(|b| b.branch_name.clone()).collect();
    assert!(branch_names_a.contains(&"a/1".to_string()));
    assert!(branch_names_a.contains(&"a/2".to_string()));

    let for_b = store.bundles().list_by_work_id(&work_b.id).await.expect("list b");
    assert_eq!(for_b.len(), 1);
    assert_eq!(for_b[0].branch_name, "b/1");
}

#[tokio::test]
async fn list_by_work_id_empty_when_none_match() {
    let (_dir, store, _work) = fresh_store_with_work().await;
    let orphan_id = WorkId::new();
    let result = store
        .bundles()
        .list_by_work_id(&orphan_id)
        .await
        .expect("list by missing work_id");
    assert!(result.is_empty());
}

#[tokio::test]
async fn persists_bundles_jsonl_under_loopr_taskstore() {
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path();
    let store = Store::open(target).await.expect("open");
    let plan = Plan::new("parent".to_string());
    store.plans().create(plan.clone()).await.expect("create plan");
    let work = Work::new(plan.id.clone(), "w".to_string());
    store.works().create(work.clone()).await.expect("create work");

    let bundle = sample_bundle(&work, "persisted");
    store.bundles().create(bundle.clone()).await.expect("create bundle");
    store.close().await.expect("close");

    let bundles_jsonl = target.join(".loopr").join("taskstore").join("bundles.jsonl");
    assert!(
        bundles_jsonl.is_file(),
        "bundles.jsonl exists at {}",
        bundles_jsonl.display()
    );
    let body = std::fs::read_to_string(&bundles_jsonl).expect("read jsonl");
    assert!(body.contains(bundle.id.as_ref()), "bundles.jsonl contains id: {body}");
    assert!(body.contains("persisted"), "branch_name round-trips");
}
