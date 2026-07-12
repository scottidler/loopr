//! Integration tests for `ReviewsStore` (Phase 7 of
//! `docs/design/2026-07-11-verified-swarm.md`). Exercises the full
//! create -> get -> list_by_bundle round-trip through JSONL + SQLite.

#![allow(clippy::unwrap_used)]

use tempfile::TempDir;

use domain::{BundleId, CheckRunId, Review, ReviewId, ReviewIssue, Severity, Verdict};
use store::{ReviewsStore, Store};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn reviews_store_is_send_sync() {
    assert_send_sync::<ReviewsStore<'static>>();
}

async fn fresh_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    (dir, store)
}

fn accept_review(bundle_id: BundleId, round: u32) -> Review {
    Review::new(
        bundle_id,
        round,
        Verdict::Accept {
            summary: "looks good".to_string(),
        },
        "looks good".to_string(),
        Vec::new(),
        vec![CheckRunId::new()],
        "claude-opus-4-8".to_string(),
    )
}

#[tokio::test]
async fn create_get_round_trips_a_change_requested_review() {
    let (_dir, store) = fresh_store().await;
    let bundle = BundleId::new();
    let reasons = vec![ReviewIssue {
        severity: Severity::Error,
        file: "src/lib.rs".to_string(),
        line: Some(42),
        message: "missing error handling".to_string(),
        suggestion: Some("propagate the Result".to_string()),
    }];
    let review = Review::new(
        bundle.clone(),
        1,
        Verdict::ChangeRequested {
            summary: "needs work".to_string(),
            reasons: reasons.clone(),
        },
        "needs work".to_string(),
        reasons.clone(),
        Vec::new(),
        "claude-opus-4-8".to_string(),
    );
    let expected_id = review.id.clone();
    let id = store.reviews().create(review).await.expect("create");
    assert_eq!(id, expected_id);

    let got = store.reviews().get(&id).await.expect("get");
    assert_eq!(got.round, 1);
    assert_eq!(got.summary, "needs work");
    assert_eq!(got.reasons, reasons);
    assert!(
        got.criteria.is_empty(),
        "criteria persists as an empty Vec until Phase 8"
    );
    assert!(matches!(got.verdict, Verdict::ChangeRequested { .. }));
}

#[tokio::test]
async fn list_by_bundle_returns_rounds_for_that_bundle_only() {
    let (_dir, store) = fresh_store().await;
    let bundle_a = BundleId::new();
    let bundle_b = BundleId::new();

    store
        .reviews()
        .create(accept_review(bundle_a.clone(), 1))
        .await
        .unwrap();
    store
        .reviews()
        .create(accept_review(bundle_a.clone(), 2))
        .await
        .unwrap();
    store
        .reviews()
        .create(accept_review(bundle_b.clone(), 1))
        .await
        .unwrap();

    let for_a = store.reviews().list_by_bundle(&bundle_a).await.unwrap();
    assert_eq!(for_a.len(), 2, "two rounds for bundle A");
    assert!(for_a.iter().all(|r| r.bundle_id == bundle_a));
    let mut rounds: Vec<u32> = for_a.iter().map(|r| r.round).collect();
    rounds.sort_unstable();
    assert_eq!(rounds, vec![1, 2]);

    let for_b = store.reviews().list_by_bundle(&bundle_b).await.unwrap();
    assert_eq!(for_b.len(), 1);
    assert_eq!(for_b[0].round, 1);
}

#[tokio::test]
async fn persists_across_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path();
    let bundle = BundleId::new();
    {
        let store = Store::open(target).await.expect("open");
        store.reviews().create(accept_review(bundle.clone(), 1)).await.unwrap();
        store.close().await.expect("close");
    }
    let jsonl = target.join(".loopr").join("taskstore").join("reviews.jsonl");
    assert!(jsonl.is_file(), "reviews.jsonl exists at {}", jsonl.display());

    let store = Store::open(target).await.expect("reopen");
    let got = store.reviews().list_by_bundle(&bundle).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].round, 1);
}

#[tokio::test]
async fn missing_id_yields_record_not_found() {
    let (_dir, store) = fresh_store().await;
    let bogus = ReviewId::new();
    let err = store.reviews().get(&bogus).await.expect_err("missing review");
    match err {
        store::StoreError::RecordNotFound { collection, .. } => {
            assert_eq!(collection, "reviews");
        }
        other => panic!("expected RecordNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn duplicate_id_yields_already_exists() {
    let (_dir, store) = fresh_store().await;
    let review = accept_review(BundleId::new(), 1);
    store.reviews().create(review.clone()).await.unwrap();
    let err = store.reviews().create(review).await.unwrap_err();
    match err {
        store::StoreError::AlreadyExists { collection, .. } => {
            assert_eq!(collection, "reviews");
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
}
