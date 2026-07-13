//! Tests for `WorksStore::update` -- Phase 5 log-level discrimination
//! (docs/design/2026-07-12-failure-paths-recovery-chain.md): an OCC Stale
//! refusal is a benign, recoverable race at this seam and logs at WARN,
//! never ERROR; any other update failure is a real failure and stays
//! ERROR. The caller (e.g. `override_work`) already owns the severity
//! verdict for a benign lost race; the store's job is just to not train
//! operators to ignore ERROR.
//!
//! Log assertions use a JSON `tracing` subscriber captured into a byte
//! buffer, mirroring `crates/loopr/src/daemon/context/transition/tests.rs`
//! (itself mirroring `crates/llm/src/metered.rs`'s `VecWriter`).

use std::sync::{Arc, Mutex};

use domain::{PlanId, Role, TargetKind, Work, WorkStatus};
use tempfile::TempDir;
use tracing_subscriber::layer::SubscriberExt;

use crate::{Store, StoreError};

// ---------------------------------------------------------------------------
// JSON tracing capture (mirrors transition/tests.rs's VecWriter)
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

async fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    (dir, store)
}

fn fresh_work() -> Work {
    Work::new(PlanId::new(), "test work".to_string())
}

/// Two writers race the same `expected_updated_at` snapshot; the loser's
/// OCC Stale refusal must log WARN, never ERROR.
#[tokio::test]
async fn update_stale_logs_warn_not_error() {
    let (_dir, store) = open_store().await;
    let work = fresh_work();
    let id = store.works().create(work.clone()).await.expect("create");
    let stored = store.works().get(&id).await.expect("get after create");
    let snapshot = stored.updated_at;

    // First writer: legal `Pending -> Ready`, wins the race.
    let mut winner = stored.clone();
    winner.transition(WorkStatus::Ready, Role::Reactor).expect("transition");
    store
        .works()
        .update(winner, snapshot, Role::Reactor, TargetKind::Normal)
        .await
        .expect("winner update");

    // Second writer: same stale snapshot, loses.
    let mut loser = stored;
    loser.transition(WorkStatus::Ready, Role::Reactor).expect("transition");

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    let err = store
        .works()
        .update(loser, snapshot, Role::Reactor, TargetKind::Normal)
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
    let work = fresh_work();

    let writer = VecWriter::default();
    let guard = tracing::subscriber::set_default(json_subscriber(writer.clone()));
    let err = store
        .works()
        .update(work, 0, Role::Reactor, TargetKind::Normal)
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
