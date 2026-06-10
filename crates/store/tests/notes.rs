//! Integration tests for `NotesStore` (Phase 7 of
//! `docs/design/2026-05-09-director-phase-2.md`).

#![allow(clippy::unwrap_used)]

use tempfile::TempDir;

use domain::{NoteId, OperatorNote, Plan, PlanId, now_millis};
use store::{NotesStore, Store};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn notes_store_is_send_sync() {
    assert_send_sync::<NotesStore<'static>>();
}

async fn fresh_store_with_plan() -> (TempDir, Store, Plan) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    let plan = Plan::new("parent plan".to_string());
    store.plans().create(plan.clone()).await.expect("create parent");
    (dir, store, plan)
}

fn sample_note(plan_id: PlanId, author: &str, message: &str) -> OperatorNote {
    OperatorNote::new(plan_id, author.to_string(), message.to_string())
}

#[tokio::test]
async fn create_returns_id_that_resolves() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let note = sample_note(plan.id.clone(), "alice", "retry the failing build");
    let expected_id = note.id.clone();
    let id = store.notes().create(note).await.expect("create");
    assert_eq!(id, expected_id);
    let got = store.notes().get(&id).await.expect("get");
    assert_eq!(got.author, "alice");
    assert_eq!(got.message, "retry the failing build");
    assert!(got.is_unread(), "fresh note must persist as unread");
}

#[tokio::test]
async fn list_unread_returns_unread_notes_only_for_the_plan() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let n1 = sample_note(plan.id.clone(), "alice", "first");
    let n2 = sample_note(plan.id.clone(), "alice", "second");
    let n1_id = n1.id.clone();
    store.notes().create(n1).await.unwrap();
    store.notes().create(n2).await.unwrap();

    let unread = store.notes().list_unread_for_plan(&plan.id).await.unwrap();
    assert_eq!(unread.len(), 2, "both fresh notes must be unread");

    // Mark one as read; only the other survives the filter.
    store.notes().mark_read(&[n1_id], now_millis()).await.unwrap();
    let unread = store.notes().list_unread_for_plan(&plan.id).await.unwrap();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].message, "second");
}

#[tokio::test]
async fn list_unread_filters_by_plan_id() {
    let (_dir, store, plan_a) = fresh_store_with_plan().await;
    let plan_b = Plan::new("other plan".to_string());
    store.plans().create(plan_b.clone()).await.unwrap();

    store
        .notes()
        .create(sample_note(plan_a.id.clone(), "alice", "for-a"))
        .await
        .unwrap();
    store
        .notes()
        .create(sample_note(plan_b.id.clone(), "alice", "for-b"))
        .await
        .unwrap();

    let unread_a = store.notes().list_unread_for_plan(&plan_a.id).await.unwrap();
    let unread_b = store.notes().list_unread_for_plan(&plan_b.id).await.unwrap();
    assert_eq!(unread_a.len(), 1);
    assert_eq!(unread_a[0].message, "for-a");
    assert_eq!(unread_b.len(), 1);
    assert_eq!(unread_b[0].message, "for-b");
}

#[tokio::test]
async fn list_unread_returns_oldest_first() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    // Mint and persist three notes in a known order. created_at is
    // monotonic via now_millis(), but adjacent calls in a fast loop
    // can land on the same millisecond — stamp explicitly to make
    // the ordering deterministic.
    let mut n1 = sample_note(plan.id.clone(), "alice", "oldest");
    let mut n2 = sample_note(plan.id.clone(), "alice", "middle");
    let mut n3 = sample_note(plan.id.clone(), "alice", "newest");
    let base = now_millis();
    n1.created_at = base - 200;
    n2.created_at = base - 100;
    n3.created_at = base;
    store.notes().create(n1).await.unwrap();
    store.notes().create(n2).await.unwrap();
    store.notes().create(n3).await.unwrap();

    let unread = store.notes().list_unread_for_plan(&plan.id).await.unwrap();
    let messages: Vec<&str> = unread.iter().map(|n| n.message.as_str()).collect();
    assert_eq!(messages, vec!["oldest", "middle", "newest"]);
}

#[tokio::test]
async fn mark_read_is_idempotent_for_already_read_notes() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let note = sample_note(plan.id.clone(), "alice", "msg");
    let id = note.id.clone();
    store.notes().create(note).await.unwrap();

    let ts1 = now_millis();
    store.notes().mark_read(std::slice::from_ref(&id), ts1).await.unwrap();
    let got = store.notes().get(&id).await.unwrap();
    assert_eq!(got.read_at, Some(ts1));

    // Second mark_read on the same id is a no-op; read_at and
    // updated_at do NOT advance.
    store
        .notes()
        .mark_read(std::slice::from_ref(&id), ts1 + 5000)
        .await
        .unwrap();
    let got = store.notes().get(&id).await.unwrap();
    assert_eq!(got.read_at, Some(ts1), "already-read note must not move read_at");
}

#[tokio::test]
async fn mark_read_floors_updated_at_strictly_above_prior() {
    // Phase 3 F9: mark_read stamps updated_at from the caller-supplied
    // clock; the store must floor it strictly above the prior on-disk
    // value so a same-ms (or skewed) write can't tie the merge driver's
    // latest-wins tie-break. Stamp the note's updated_at into the future
    // so now_millis() loses to prior + 1, forcing the floor branch.
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let mut note = sample_note(plan.id.clone(), "alice", "msg");
    let future = now_millis() + 1_000_000;
    note.updated_at = future;
    note.created_at = future;
    let id = note.id.clone();
    store.notes().create(note).await.unwrap();

    // Pass a stale ts_ms (in the past); read_at takes it, but updated_at
    // must still floor to prior + 1.
    store.notes().mark_read(std::slice::from_ref(&id), 1).await.unwrap();
    let got = store.notes().get(&id).await.unwrap();
    assert_eq!(got.read_at, Some(1), "read_at keeps the semantic ts_ms");
    assert_eq!(got.updated_at, future + 1, "updated_at floors to prior + 1");
}

#[tokio::test]
async fn notes_collection_name_is_operatornotes() {
    // Phase 3 F13: the on-disk collection is `operatornotes` (struct
    // ident lowercased + pluralized, no snake_case transform). Pin both
    // the JSONL filename and the error-label spelling so the two never
    // drift apart again.
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path();
    let store = Store::open(target).await.expect("open");
    let plan = Plan::new("p".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    store
        .notes()
        .create(sample_note(plan.id.clone(), "alice", "m"))
        .await
        .unwrap();
    store.close().await.expect("close");

    let jsonl = target.join(".loopr").join("taskstore").join("operatornotes.jsonl");
    assert!(jsonl.is_file(), "operatornotes.jsonl exists at {}", jsonl.display());

    // The error label must use the same spelling as the on-disk file.
    let store = Store::open(target).await.expect("reopen");
    let bogus = NoteId::new();
    let err = store.notes().get(&bogus).await.expect_err("missing note");
    match err {
        store::StoreError::RecordNotFound { collection, .. } => {
            assert_eq!(collection, "operatornotes");
        }
        other => panic!("expected RecordNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn mark_read_missing_id_errors() {
    let (_dir, store, _plan) = fresh_store_with_plan().await;
    let bogus = NoteId::new();
    let err = store.notes().mark_read(&[bogus], now_millis()).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not found") || msg.contains("RecordNotFound") || msg.contains("operator_notes"),
        "unexpected error message: {msg}"
    );
}

#[tokio::test]
async fn already_existing_id_yields_error() {
    let (_dir, store, plan) = fresh_store_with_plan().await;
    let note = sample_note(plan.id.clone(), "alice", "msg");
    store.notes().create(note.clone()).await.unwrap();
    let err = store.notes().create(note).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("already") || msg.contains("AlreadyExists"),
        "expected AlreadyExists, got: {msg}"
    );
}
