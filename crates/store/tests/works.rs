use std::str::FromStr;

use tempfile::TempDir;

use domain::{AcceptanceCriteria, Plan, Work, WorkId, WorkStatus};
use store::{Store, StoreError, WorksStore};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn works_store_is_send_sync() {
    assert_send_sync::<WorksStore<'static>>();
}

async fn fresh_store_with_plan() -> (TempDir, Store, Plan) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    let plan = Plan::new("parent plan".to_string());
    store.plans().create(plan.clone()).await.expect("create parent");
    (dir, store, plan)
}

fn sample_work(parent: &Plan, title: &str) -> Work {
    let mut w = Work::new(parent.id.clone(), title.to_string());
    w.acceptance_criteria = AcceptanceCriteria(vec![format!("assert {title} works")]);
    w
}

#[tokio::test]
async fn empty_works_list_returns_empty_vec() {
    let (_dir, store, _plan) = fresh_store_with_plan().await;
    let works = store.works().list().await.expect("list");
    assert!(works.is_empty());
}

#[tokio::test]
async fn create_returns_id_that_resolves() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let work = sample_work(&plan, "build cli");
    let expected_id = work.id.clone();
    let id = store.works().create(work.clone()).await.expect("create");
    assert_eq!(id, expected_id);
    let got = store.works().get(&id).await.expect("get");
    assert_eq!(got.id, work.id);
    assert_eq!(got.title, "build cli");
    assert_eq!(got.parent_id, plan.id);
    assert_eq!(got.status, WorkStatus::Pending);
}

#[tokio::test]
async fn create_then_list_sees_record() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let work = sample_work(&plan, "only child");
    store.works().create(work.clone()).await.expect("create");
    let listed = store.works().list().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "only child");
}

#[tokio::test]
async fn create_many_persists_batch_in_single_call() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let works = vec![
        sample_work(&plan, "first"),
        sample_work(&plan, "second"),
        sample_work(&plan, "third"),
    ];
    let expected_ids: Vec<WorkId> = works.iter().map(|w| w.id.clone()).collect();
    let returned = store.works().create_many(works).await.expect("create_many");
    assert_eq!(returned.len(), 3);
    for id in &expected_ids {
        assert!(returned.contains(id), "expected id {id} in returned vec");
    }

    let listed = store.works().list().await.expect("list");
    assert_eq!(listed.len(), 3);
    let titles: Vec<&str> = listed.iter().map(|w| w.title.as_str()).collect();
    assert!(titles.contains(&"first"));
    assert!(titles.contains(&"second"));
    assert!(titles.contains(&"third"));
}

#[tokio::test]
async fn create_many_empty_vec_is_noop() {
    let (_dir, store, _plan) = fresh_store_with_plan().await;
    let returned = store.works().create_many(vec![]).await.expect("create_many empty");
    assert!(returned.is_empty());
    let listed = store.works().list().await.expect("list");
    assert!(listed.is_empty());
}

#[tokio::test]
async fn create_same_id_twice_returns_already_exists() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let w1 = sample_work(&plan, "first");
    let w2 = Work {
        id: w1.id.clone(),
        parent_id: plan.id.clone(),
        updated_at: w1.updated_at,
        created_at: w1.created_at,
        title: "second-with-same-id".to_string(),
        assignee: None,
        status: WorkStatus::Pending,
        dependencies: vec![],
        files: vec![],
        acceptance_criteria: AcceptanceCriteria(vec!["assert nope".to_string()]),
        attempt_count: 0,
        session_failure_count: 0,
    };
    store.works().create(w1).await.expect("first create");
    let err = store.works().create(w2).await.expect_err("second should reject");
    match err {
        StoreError::AlreadyExists { collection, id: _ } => {
            assert_eq!(collection, "works");
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    let listed = store.works().list().await.expect("list");
    assert_eq!(listed.len(), 1, "duplicate should not have overwritten");
}

#[tokio::test]
async fn get_missing_returns_record_not_found() {
    let (_dir, store, _plan) = fresh_store_with_plan().await;
    let nonexistent = WorkId::from_str("wk-xxxxx").expect("infallible");
    let err = store.works().get(&nonexistent).await.expect_err("should miss");
    match err {
        StoreError::RecordNotFound { collection, id } => {
            assert_eq!(collection, "works");
            assert_eq!(id, "wk-xxxxx");
        }
        other => panic!("expected RecordNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn persists_works_jsonl_under_loopr_taskstore() {
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path();
    let store = Store::open(target).await.expect("open");
    let plan = Plan::new("parent".to_string());
    store.plans().create(plan.clone()).await.expect("create plan");

    let work = sample_work(&plan, "persisted");
    store.works().create(work.clone()).await.expect("create work");
    store.close().await.expect("close");

    let works_jsonl = target.join(".loopr").join("taskstore").join("works.jsonl");
    assert!(works_jsonl.is_file(), "works.jsonl exists at {}", works_jsonl.display());
    let body = std::fs::read_to_string(&works_jsonl).expect("read jsonl");
    assert!(body.contains(work.id.as_ref()), "works.jsonl contains id: {body}");
    assert!(body.contains("persisted"), "title round-trips");
}

#[tokio::test]
async fn serde_roundtrip_across_statuses() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let cases = [
        WorkStatus::Pending,
        WorkStatus::Ready,
        WorkStatus::InProgress,
        WorkStatus::Done,
        WorkStatus::Abandoned,
    ];
    for status in cases {
        let mut work = sample_work(&plan, &format!("title-{status}"));
        work.status = status;
        let id = store.works().create(work.clone()).await.expect("create");
        let got = store.works().get(&id).await.expect("get");
        assert_eq!(got.status, status, "status roundtrips for {status}");
        assert_eq!(got.acceptance_criteria.len(), 1);
    }
}
