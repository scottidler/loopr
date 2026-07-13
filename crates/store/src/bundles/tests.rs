//! Tests for `BundlesStore` — round-trip CRUD and OCC update
//! behaviour.
//!
//! The OCC tests are the interesting ones: they pin the intra-daemon
//! race-defense invariant. See
//! `docs/design/2026-04-22-reviewer.md` -> "Intra-daemon OCC via
//! Mutex + updated_at version check" for the rationale.

use std::sync::{Arc, Mutex};

use domain::{Bundle, BundleId, BundleStatus, Role, TargetKind, WorkId};
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt;

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

/// Phase 3 of `docs/design/2026-07-12-failure-paths-recovery-chain.md`
/// (folds in OCC doc Phase 2): pins the same-millisecond floor window that
/// doomed the reviewer (`docs/design/2026-07-12-reviewer-occ-stale-race.md`,
/// Link 1, `5dfd3112`). Forces `current.updated_at` into the future so the
/// triage write's floor deterministically lands at `current.updated_at + 1`
/// (same idiom as `update_floors_updated_at_strictly_above_prior`) regardless
/// of wall-clock timing — this reproduces "create + triage land in the same
/// millisecond" without depending on scheduler luck. A caller that syncs its
/// local copy to the RETURNED floored value (the fix) then succeeds on the
/// immediately-following review-transition. Break-to-proven: temporarily
/// swap the last argument below from `floored` to `unsynced_local_ts` (the
/// pre-fix discard shape — keeping the caller's pre-write snapshot instead of
/// the returned value) and this test goes red with `StoreError::Stale`.
#[tokio::test]
async fn review_transition_succeeds_on_synced_copy_across_same_millisecond_floor() {
    let (_dir, store) = open_store().await;
    let mut bundle = fresh_bundle();
    let future = domain::now_millis() + 1_000_000;
    bundle.updated_at = future;
    bundle.created_at = future;
    let id = store.bundles().create(bundle.clone()).await.expect("create");

    // Immediate triage: `Bundle::transition` stamps `now_millis()` locally,
    // far behind `future` — this is the daemon's pre-write snapshot, the
    // value the pre-fix discard shape kept instead of the store's return.
    let mut triaged = bundle.clone();
    triaged
        .transition(BundleStatus::Triaged, Role::Reactor)
        .expect("triage");
    let unsynced_local_ts = triaged.updated_at;

    let floored = store
        .bundles()
        .update(triaged.clone(), future, Role::Reactor, TargetKind::Normal)
        .await
        .expect("triage persist");
    assert_eq!(floored, future + 1, "floor lands exactly at prior + 1");
    assert!(
        unsynced_local_ts < floored,
        "local transition timestamp must lag the floored value to reproduce the same-ms window"
    );

    // The fix: sync the local copy to the returned floored value before the
    // next write snapshots its OCC token from it.
    let mut synced = triaged.clone();
    synced.updated_at = floored;
    let expected_updated_at = synced.updated_at; // the fix: synced, not the pre-write snapshot
    let mut review = synced;
    review
        .transition(BundleStatus::Reviewed, Role::Reviewer)
        .expect("review");
    store
        .bundles()
        .update(review, expected_updated_at, Role::Reviewer, TargetKind::Normal)
        .await
        .expect("review transition on synced copy must succeed");

    let after = store.bundles().get(&id).await.expect("get");
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

// ---------------------------------------------------------------------------
// Phase 5 (docs/design/2026-07-12-failure-paths-recovery-chain.md):
// store-seam log-level discrimination. An OCC Stale refusal is a benign,
// recoverable race at this seam and logs at WARN, never ERROR; any other
// update failure is a real failure and stays ERROR. Sibling of
// `crates/store/src/works/tests.rs`'s identical coverage.
//
// Log assertions use a JSON `tracing` subscriber captured into a byte
// buffer, mirroring `crates/loopr/src/daemon/context/transition/tests.rs`
// (itself mirroring `crates/llm/src/metered.rs`'s `VecWriter`).
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct VecWriter(Arc<Mutex<Vec<u8>>>);

impl VecWriter {
    fn snapshot(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }
}

impl std::io::Write for VecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
    type Writer = VecWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn json_subscriber(writer: VecWriter) -> impl tracing::Subscriber + Send + Sync {
    let layer = tracing_subscriber::fmt::layer().json().with_writer(writer);
    tracing_subscriber::registry().with(layer)
}

/// Count JSON log lines at `level` whose message contains `needle`.
fn count_lines(json: &str, level: &str, needle: &str) -> usize {
    json.lines()
        .filter(|line| {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            v.get("level").and_then(|l| l.as_str()) == Some(level)
                && v.get("fields")
                    .and_then(|f| f.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|m| m.contains(needle))
                    .unwrap_or(false)
        })
        .count()
}

const STALE_NEEDLE: &str = "OCC Stale refusal";
const FAILED_NEEDLE: &str = "update failed";

/// Two writers race the same `expected_updated_at` snapshot; the loser's
/// OCC Stale refusal must log WARN, never ERROR.
#[tokio::test]
async fn update_stale_logs_warn_not_error() {
    let (_dir, store) = open_store().await;
    let (_id, triaged) = create_triaged(&store).await;
    let snapshot = triaged.updated_at;

    // First writer: legal `Triaged -> Reviewed`, wins the race.
    let mut winner = triaged.clone();
    winner
        .transition(BundleStatus::Reviewed, Role::Reviewer)
        .expect("review");
    store
        .bundles()
        .update(winner, snapshot, Role::Reviewer, TargetKind::Normal)
        .await
        .expect("winner update");

    // Second writer: same stale snapshot, loses.
    let mut loser = triaged;
    loser
        .transition(BundleStatus::Rejected, Role::Reviewer)
        .expect("reject");

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    let err = store
        .bundles()
        .update(loser, snapshot, Role::Reviewer, TargetKind::Normal)
        .await
        .unwrap_err();
    drop(guard);

    assert!(matches!(err, StoreError::Stale { .. }), "expected Stale, got {err:?}");
    let log = writer.snapshot();
    assert_eq!(
        count_lines(&log, "WARN", STALE_NEEDLE),
        1,
        "expected exactly one WARN for the Stale refusal; log: {log}"
    );
    assert_eq!(
        count_lines(&log, "ERROR", STALE_NEEDLE),
        0,
        "Stale must never log ERROR; log: {log}"
    );
}

/// A non-Stale update failure (missing record) stays ERROR.
#[tokio::test]
async fn update_non_stale_failure_logs_error() {
    let (_dir, store) = open_store().await;
    let bundle = fresh_bundle();

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    let err = store
        .bundles()
        .update(bundle, 0, Role::Reactor, TargetKind::Normal)
        .await
        .unwrap_err();
    drop(guard);

    assert!(
        matches!(err, StoreError::RecordNotFound { .. }),
        "expected RecordNotFound, got {err:?}"
    );
    let log = writer.snapshot();
    assert_eq!(
        count_lines(&log, "ERROR", FAILED_NEEDLE),
        1,
        "expected exactly one ERROR for the non-Stale failure; log: {log}"
    );
    assert_eq!(
        count_lines(&log, "WARN", STALE_NEEDLE),
        0,
        "must not emit the Stale WARN path; log: {log}"
    );
}
