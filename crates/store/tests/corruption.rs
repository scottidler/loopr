//! Integration tests for `BundlesStore::list_tolerant` /
//! `WorksStore::list_tolerant`. The wrappers forward to
//! `taskstore_async::AsyncStore::list_tolerant` which reads the JSONL
//! file directly (bypassing the SQLite cache) and returns every line
//! that parsed plus a sidecar list of every line that did not.
//!
//! Phase 2 of the Tier-1 cleanup batch wires these into the daemon's
//! reconcile sweep so a corrupt JSONL row surfaces as a tracked
//! `corruption_count` instead of a silent drop at `sync()`. These
//! tests exercise the contract from the store side.

use std::str::FromStr;

use tempfile::TempDir;

use domain::{Bundle, Plan, Work, WorkId};
use store::{Store, TASKSTORE_SUBPATH};

async fn fresh_store_with_work() -> (TempDir, Store, Work) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    let plan = Plan::new("parent plan".to_string());
    store.plans().create(plan.clone()).await.expect("create plan");
    let work = Work::new(plan.id.clone(), "do a thing".to_string());
    store.works().create(work.clone()).await.expect("create work");
    (dir, store, work)
}

fn jsonl_path(target: &std::path::Path, collection: &str) -> std::path::PathBuf {
    target.join(TASKSTORE_SUBPATH).join(format!("{collection}.jsonl"))
}

fn append_raw_line(path: &std::path::Path, line: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open jsonl for append");
    writeln!(f, "{line}").expect("write raw line");
}

#[tokio::test]
async fn list_tolerant_bundles_separates_records_from_corruption() {
    let (dir, store, work) = fresh_store_with_work().await;
    let bundle = Bundle::new(work.id.clone(), "wk-abc/1".to_string(), vec!["it works".to_string()]);
    store.bundles().create(bundle).await.expect("create bundle");

    // Append one definitively corrupt line: not valid JSON.
    let bundles_jsonl = jsonl_path(dir.path(), "bundles");
    append_raw_line(&bundles_jsonl, "{ this is not json");

    let result = store.bundles().list_tolerant(&[]).await.expect("list_tolerant");
    assert_eq!(result.records.len(), 1, "valid record should still parse");
    assert_eq!(
        result.corruption.len(),
        1,
        "corrupt line should surface as one CorruptionEntry"
    );
    let entry = &result.corruption[0];
    assert_eq!(entry.file, bundles_jsonl);
}

#[tokio::test]
async fn list_tolerant_works_separates_records_from_corruption() {
    let (dir, store, _work) = fresh_store_with_work().await;

    let works_jsonl = jsonl_path(dir.path(), "works");
    // The fresh store has one valid Work already; append a corrupt row.
    append_raw_line(&works_jsonl, "not valid json at all");

    let result = store.works().list_tolerant(&[]).await.expect("list_tolerant");
    assert_eq!(result.records.len(), 1, "valid Work should still parse");
    assert_eq!(result.corruption.len(), 1, "corrupt line surfaces");
    assert_eq!(result.corruption[0].file, works_jsonl);
}

#[tokio::test]
async fn list_tolerant_bundles_clean_store_has_zero_corruption() {
    let (_dir, store, work) = fresh_store_with_work().await;
    let bundle = Bundle::new(work.id.clone(), "wk-abc/1".to_string(), vec!["it works".to_string()]);
    store.bundles().create(bundle).await.expect("create bundle");

    let result = store.bundles().list_tolerant(&[]).await.expect("list_tolerant");
    assert_eq!(result.records.len(), 1);
    assert!(result.corruption.is_empty());
}

#[tokio::test]
async fn list_tolerant_works_missing_id_field_counts_as_corruption() {
    let (dir, store, _work) = fresh_store_with_work().await;

    let works_jsonl = jsonl_path(dir.path(), "works");
    // Valid JSON but missing `id` field — the JSONL reader rejects it.
    append_raw_line(&works_jsonl, r#"{"name": "no id here"}"#);

    let result = store.works().list_tolerant(&[]).await.expect("list_tolerant");
    assert_eq!(result.records.len(), 1, "valid Work still parses");
    assert_eq!(result.corruption.len(), 1, "missing-id row surfaces as corruption");
}

// Sanity check on the unused-import warning: `WorkId::from_str` is exercised
// elsewhere in the suite; pull a token use here so a future refactor that
// drops the import does not need to be guessed at.
#[test]
fn work_id_from_str_smoke() {
    let _ = WorkId::from_str("wk-abc12").expect("infallible");
}
