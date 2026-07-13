//! Tests for `discriminate_stale_bundle_write` — the shared three-way OCC
//! Stale discriminator used byte-identically by both daemon arms: the
//! reviewer-result arm (`context.rs`, expected status `Triaged`) and
//! `accept_bundle` (`spawner.rs`, expected status `Reviewed`). Both arms
//! funnel into this one helper differing ONLY by the `expected` argument,
//! so exercising the helper with each of those two statuses IS the
//! per-arm coverage (Phase 4 of
//! `docs/design/2026-07-12-failure-paths-recovery-chain.md`; folds in the
//! OCC doc's Phase 3 enumeration).
//!
//! Log assertions use a JSON `tracing` subscriber captured into a byte
//! buffer (same pattern as `crates/llm/src/metered.rs`'s `VecWriter`), so
//! the loud `error!` branches are verified by the exact line they emit —
//! break-to-proven against the pre-fix silent-swallow shape (see the
//! implementation notes' break-to-proven section).

use std::sync::{Arc, Mutex};

use domain::{Bundle, BundleId, BundleStatus, Review, Role, TargetKind, Verdict, WorkId};
use store::Store;
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt;

use super::discriminate_stale_bundle_write;

// ---------------------------------------------------------------------------
// JSON tracing capture (mirrors crates/llm/src/metered.rs::VecWriter)
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

/// The first JSON log line at `level` whose message contains `needle`, as a
/// parsed `serde_json::Value`. `None` if no such line exists.
fn find_line(json: &str, level: &str, needle: &str) -> Option<serde_json::Value> {
    json.lines().find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let is_level = v.get("level").and_then(|l| l.as_str()) == Some(level);
        let msg_hit = v
            .get("fields")
            .and_then(|f| f.get("message"))
            .and_then(|m| m.as_str())
            .map(|m| m.contains(needle))
            .unwrap_or(false);
        (is_level && msg_hit).then_some(v)
    })
}

/// Count JSON log lines at `level` whose message contains `needle`.
fn count_lines(json: &str, level: &str, needle: &str) -> usize {
    json.lines().filter(|line| find_one(line, level, needle)).count()
}

fn find_one(line: &str, level: &str, needle: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    v.get("level").and_then(|l| l.as_str()) == Some(level)
        && v.get("fields")
            .and_then(|f| f.get("message"))
            .and_then(|m| m.as_str())
            .map(|m| m.contains(needle))
            .unwrap_or(false)
}

const INVARIANT_NEEDLE: &str = "OCC invariant violation, no winner";
const WINNER_NEEDLE: &str = "another writer won";
const REREAD_NEEDLE: &str = "re-read failed";

// ---------------------------------------------------------------------------
// Store fixtures: put a Bundle on disk at an exact status.
// ---------------------------------------------------------------------------

async fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open store");
    (dir, store)
}

/// Persist a Bundle at `Proposed` and return its id + the in-memory copy
/// synced to the on-disk `updated_at`.
async fn create_proposed(store: &Store) -> (BundleId, Bundle) {
    let bundle = Bundle::new(
        WorkId::new(),
        "loopr/test-branch".to_string(),
        vec!["claim".to_string()],
    );
    let id = store.bundles().create(bundle.clone()).await.expect("create");
    let stored = store.bundles().get(&id).await.expect("get after create");
    (id, stored)
}

/// Advance an in-memory Bundle one legal FSM edge and persist it, returning
/// the copy synced to the returned floored `updated_at`.
async fn advance(store: &Store, mut bundle: Bundle, target: BundleStatus, role: Role) -> Bundle {
    let expected = bundle.updated_at;
    bundle.transition(target, role).expect("legal transition");
    let ts = store
        .bundles()
        .update(bundle.clone(), expected, role, TargetKind::Normal)
        .await
        .expect("persist");
    bundle.updated_at = ts;
    bundle
}

async fn create_at(store: &Store, target: BundleStatus) -> (BundleId, Bundle) {
    let (id, proposed) = create_proposed(store).await;
    let mut bundle = advance(store, proposed, BundleStatus::Triaged, Role::Reactor).await;
    if target == BundleStatus::Triaged {
        return (id, bundle);
    }
    bundle = advance(store, bundle, BundleStatus::Reviewed, Role::Reviewer).await;
    if target == BundleStatus::Reviewed {
        return (id, bundle);
    }
    bundle = advance(store, bundle, BundleStatus::Accepted, Role::Director).await;
    assert_eq!(target, BundleStatus::Accepted, "create_at only builds up to Accepted");
    (id, bundle)
}

/// Append `n` Review rows (rounds 1..=n) for a Bundle so the loud path has
/// a real round to report.
async fn seed_reviews(store: &Store, bundle_id: &BundleId, n: u32) {
    for round in 1..=n {
        let review = Review::new(
            bundle_id.clone(),
            round,
            Verdict::Reject {
                reason: format!("round {round}"),
            },
            format!("round {round}"),
            Vec::new(),
            Vec::new(),
            "test-model".to_string(),
        );
        store.reviews().create(review).await.expect("create review");
    }
}

fn json_subscriber(writer: VecWriter) -> impl tracing::Subscriber + Send + Sync {
    let layer = tracing_subscriber::fmt::layer().json().with_writer(writer);
    tracing_subscriber::registry().with(layer)
}

// ---------------------------------------------------------------------------
// F6 arm (reviewer-result): expected status == Triaged.
// ---------------------------------------------------------------------------

/// Not advanced: the Bundle is still `Triaged` after the reviewer's write
/// lost to a Stale — an OCC invariant violation with no winner. Loud
/// `error!` carrying bundle id, both Stale timestamps, and the round.
///
/// Break-to-proven: this is the loud branch. The pre-fix shape (a bare
/// `debug!` swallow, no re-read) emits NO ERROR line, so the
/// `INVARIANT_NEEDLE` count would be 0 and this assertion fails. Verified
/// by temporarily restoring that shape (see implementation notes).
#[tokio::test]
async fn f6_still_triaged_is_loud_invariant_violation() {
    let (_dir, store) = open_store().await;
    let (id, _bundle) = create_at(&store, BundleStatus::Triaged).await;
    seed_reviews(&store, &id, 2).await;

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    discriminate_stale_bundle_write(&store, &id, BundleStatus::Triaged, 111, 222).await;
    drop(guard);

    let log = writer.snapshot();
    assert_eq!(
        count_lines(&log, "ERROR", INVARIANT_NEEDLE),
        1,
        "expected exactly one loud invariant-violation ERROR; log: {log}"
    );
    assert_eq!(
        count_lines(&log, "DEBUG", WINNER_NEEDLE),
        0,
        "must not claim a winner; log: {log}"
    );

    let line = find_line(&log, "ERROR", INVARIANT_NEEDLE).expect("loud line present");
    let fields = &line["fields"];
    assert_eq!(
        fields["bundle_id"].as_str(),
        Some(id.to_string().as_str()),
        "carries bundle id"
    );
    assert_eq!(fields["stale_expected"].as_i64(), Some(111), "carries expected ts");
    assert_eq!(fields["stale_actual"].as_i64(), Some(222), "carries actual ts");
    assert_eq!(fields["round"].as_u64(), Some(2), "carries the latest review round");
    assert_eq!(
        fields["expected_status"].as_str(),
        Some("Triaged"),
        "names the expected status"
    );
}

/// Advanced: the Bundle moved on to `Reviewed` — a legitimate lost race.
/// Silent `debug!` winner, NO `error!`.
#[tokio::test]
async fn f6_advanced_to_reviewed_is_silent_winner() {
    let (_dir, store) = open_store().await;
    let (id, _bundle) = create_at(&store, BundleStatus::Reviewed).await;

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    discriminate_stale_bundle_write(&store, &id, BundleStatus::Triaged, 111, 222).await;
    drop(guard);

    let log = writer.snapshot();
    assert_eq!(
        count_lines(&log, "ERROR", INVARIANT_NEEDLE),
        0,
        "benign race must not fail loud; log: {log}"
    );
    assert_eq!(
        count_lines(&log, "ERROR", REREAD_NEEDLE),
        0,
        "re-read succeeded; log: {log}"
    );
    assert_eq!(
        count_lines(&log, "DEBUG", WINNER_NEEDLE),
        1,
        "expected the silent winner debug; log: {log}"
    );
    let line = find_line(&log, "DEBUG", WINNER_NEEDLE).expect("winner line present");
    assert_eq!(
        line["fields"]["winner_status"].as_str(),
        Some("Reviewed"),
        "names the winner status"
    );
}

/// Re-read fails: the Bundle id does not exist, so the re-read errors and
/// the helper cannot tell a lost race from a violation — fail loud with the
/// re-read error plus both Stale timestamps.
#[tokio::test]
async fn f6_reread_failure_is_loud() {
    let (_dir, store) = open_store().await;
    let missing = BundleId::new();

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    discriminate_stale_bundle_write(&store, &missing, BundleStatus::Triaged, 111, 222).await;
    drop(guard);

    let log = writer.snapshot();
    assert_eq!(
        count_lines(&log, "ERROR", REREAD_NEEDLE),
        1,
        "expected the loud re-read-failure ERROR; log: {log}"
    );
    assert_eq!(
        count_lines(&log, "ERROR", INVARIANT_NEEDLE),
        0,
        "not the invariant branch; log: {log}"
    );
    let line = find_line(&log, "ERROR", REREAD_NEEDLE).expect("reread line present");
    let fields = &line["fields"];
    assert_eq!(fields["stale_expected"].as_i64(), Some(111), "carries expected ts");
    assert_eq!(fields["stale_actual"].as_i64(), Some(222), "carries actual ts");
    assert!(
        fields.get("reread_error").is_some(),
        "carries the re-read error; log: {log}"
    );
}

// ---------------------------------------------------------------------------
// accept_bundle arm: expected status == Reviewed.
// ---------------------------------------------------------------------------

/// Not advanced: still `Reviewed` after `accept_bundle`'s
/// `Reviewed -> Accepted` write lost to a Stale — invariant violation, loud.
///
/// Break-to-proven: the loud branch; the pre-fix silent-swallow shape emits
/// no ERROR and this assertion fails (see implementation notes).
#[tokio::test]
async fn accept_still_reviewed_is_loud_invariant_violation() {
    let (_dir, store) = open_store().await;
    let (id, _bundle) = create_at(&store, BundleStatus::Reviewed).await;
    seed_reviews(&store, &id, 1).await;

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    discriminate_stale_bundle_write(&store, &id, BundleStatus::Reviewed, 333, 444).await;
    drop(guard);

    let log = writer.snapshot();
    assert_eq!(
        count_lines(&log, "ERROR", INVARIANT_NEEDLE),
        1,
        "expected exactly one loud invariant-violation ERROR; log: {log}"
    );
    assert_eq!(
        count_lines(&log, "DEBUG", WINNER_NEEDLE),
        0,
        "must not claim a winner; log: {log}"
    );
    let line = find_line(&log, "ERROR", INVARIANT_NEEDLE).expect("loud line present");
    let fields = &line["fields"];
    assert_eq!(
        fields["bundle_id"].as_str(),
        Some(id.to_string().as_str()),
        "carries bundle id"
    );
    assert_eq!(fields["stale_expected"].as_i64(), Some(333), "carries expected ts");
    assert_eq!(fields["stale_actual"].as_i64(), Some(444), "carries actual ts");
    assert_eq!(fields["round"].as_u64(), Some(1), "carries the latest review round");
    assert_eq!(
        fields["expected_status"].as_str(),
        Some("Reviewed"),
        "names the expected status"
    );
}

/// Advanced: the Bundle moved on to `Accepted` — a legitimate lost race.
/// Silent `debug!` winner, NO `error!`.
#[tokio::test]
async fn accept_advanced_to_accepted_is_silent_winner() {
    let (_dir, store) = open_store().await;
    let (id, _bundle) = create_at(&store, BundleStatus::Accepted).await;

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    discriminate_stale_bundle_write(&store, &id, BundleStatus::Reviewed, 333, 444).await;
    drop(guard);

    let log = writer.snapshot();
    assert_eq!(
        count_lines(&log, "ERROR", INVARIANT_NEEDLE),
        0,
        "benign race must not fail loud; log: {log}"
    );
    assert_eq!(
        count_lines(&log, "DEBUG", WINNER_NEEDLE),
        1,
        "expected the silent winner debug; log: {log}"
    );
    let line = find_line(&log, "DEBUG", WINNER_NEEDLE).expect("winner line present");
    assert_eq!(
        line["fields"]["winner_status"].as_str(),
        Some("Accepted"),
        "names the winner status"
    );
}

/// Re-read fails on the accept arm too — identical loud outcome to F6.
#[tokio::test]
async fn accept_reread_failure_is_loud() {
    let (_dir, store) = open_store().await;
    let missing = BundleId::new();

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    discriminate_stale_bundle_write(&store, &missing, BundleStatus::Reviewed, 333, 444).await;
    drop(guard);

    let log = writer.snapshot();
    assert_eq!(
        count_lines(&log, "ERROR", REREAD_NEEDLE),
        1,
        "expected the loud re-read-failure ERROR; log: {log}"
    );
    assert_eq!(
        count_lines(&log, "ERROR", INVARIANT_NEEDLE),
        0,
        "not the invariant branch; log: {log}"
    );
    let line = find_line(&log, "ERROR", REREAD_NEEDLE).expect("reread line present");
    assert!(
        line["fields"].get("reread_error").is_some(),
        "carries the re-read error; log: {log}"
    );
}
