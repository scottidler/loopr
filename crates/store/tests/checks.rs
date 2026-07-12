//! Integration tests for `CheckRunsStore` (Phase 7 of
//! `docs/design/2026-07-11-verified-swarm.md`). Exercises the full
//! create -> get -> list_by_bundle round-trip through JSONL + SQLite.

#![allow(clippy::unwrap_used)]

use tempfile::TempDir;

use domain::{BundleId, CheckRun, CheckRunId, Role, WorkId};
use store::{CheckRunsStore, Store};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn check_runs_store_is_send_sync() {
    assert_send_sync::<CheckRunsStore<'static>>();
}

async fn fresh_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    (dir, store)
}

fn sample(bundle_id: BundleId, work_id: WorkId, command: &str, exit_code: i32) -> CheckRun {
    CheckRun::new(
        bundle_id,
        work_id,
        command.to_string(),
        exit_code,
        format!("digest-of-{command}"),
        "output tail".to_string(),
        Role::Reviewer,
        500,
    )
}

#[tokio::test]
async fn create_get_round_trips() {
    let (_dir, store) = fresh_store().await;
    let cr = sample(BundleId::new(), WorkId::new(), "cargo test", 0);
    let expected_id = cr.id.clone();
    let id = store.check_runs().create(cr).await.expect("create");
    assert_eq!(id, expected_id);
    let got = store.check_runs().get(&id).await.expect("get");
    assert_eq!(got.command, "cargo test");
    assert_eq!(got.exit_code, 0);
    assert_eq!(got.executor, Role::Reviewer);
    assert!(got.passed());
}

#[tokio::test]
async fn list_by_bundle_returns_only_matching_bundle() {
    let (_dir, store) = fresh_store().await;
    let bundle_a = BundleId::new();
    let bundle_b = BundleId::new();
    let work = WorkId::new();

    store
        .check_runs()
        .create(sample(bundle_a.clone(), work.clone(), "cargo test", 0))
        .await
        .unwrap();
    store
        .check_runs()
        .create(sample(bundle_a.clone(), work.clone(), "cargo clippy", 1))
        .await
        .unwrap();
    store
        .check_runs()
        .create(sample(bundle_b.clone(), work.clone(), "cargo test", 0))
        .await
        .unwrap();

    let for_a = store.check_runs().list_by_bundle(&bundle_a).await.unwrap();
    assert_eq!(for_a.len(), 2, "both checks for bundle A");
    assert!(for_a.iter().all(|c| c.bundle_id == bundle_a));

    let for_b = store.check_runs().list_by_bundle(&bundle_b).await.unwrap();
    assert_eq!(for_b.len(), 1);
    assert_eq!(for_b[0].command, "cargo test");
}

#[tokio::test]
async fn persists_across_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path();
    let bundle = BundleId::new();
    {
        let store = Store::open(target).await.expect("open");
        store
            .check_runs()
            .create(sample(bundle.clone(), WorkId::new(), "cargo test", 101))
            .await
            .unwrap();
        store.close().await.expect("close");
    }
    // JSONL is the source of truth: reopen and the row survives.
    let jsonl = target.join(".loopr").join("taskstore").join("checkruns.jsonl");
    assert!(jsonl.is_file(), "checkruns.jsonl exists at {}", jsonl.display());

    let store = Store::open(target).await.expect("reopen");
    let got = store.check_runs().list_by_bundle(&bundle).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].exit_code, 101);
}

#[tokio::test]
async fn missing_id_yields_record_not_found() {
    let (_dir, store) = fresh_store().await;
    let bogus = CheckRunId::new();
    let err = store.check_runs().get(&bogus).await.expect_err("missing check run");
    match err {
        store::StoreError::RecordNotFound { collection, .. } => {
            assert_eq!(collection, "checkruns");
        }
        other => panic!("expected RecordNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_id_yields_already_exists() {
    let (_dir, store) = fresh_store().await;
    let cr = sample(BundleId::new(), WorkId::new(), "cargo test", 0);
    store.check_runs().create(cr.clone()).await.unwrap();
    let err = store.check_runs().create(cr).await.unwrap_err();
    match err {
        store::StoreError::AlreadyExists { collection, .. } => {
            assert_eq!(collection, "checkruns");
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
}
