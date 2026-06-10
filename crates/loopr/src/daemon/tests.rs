use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use tokio::task::JoinSet;

use super::{bound_startup, drain_pool, reap_finished};
use crate::error::LooprError;

/// Read `events.log` JSONL lines from a test run dir.
fn read_events(run_dir: &Path) -> Vec<Value> {
    let body = std::fs::read_to_string(run_dir.join("events.log")).expect("read events.log");
    body.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("parse JSONL"))
        .collect()
}

fn has_event_with_pool(events: &[Value], message: &str, pool: &str) -> bool {
    events.iter().any(|ev| {
        let f = ev.get("fields");
        f.and_then(|f| f.get("message")).and_then(Value::as_str) == Some(message)
            && f.and_then(|f| f.get("pool")).and_then(Value::as_str) == Some(pool)
    })
}

#[tokio::test]
async fn bound_startup_passes_through_ok() {
    let result = bound_startup(Duration::from_secs(60), async { Ok::<_, LooprError>(42) }).await;
    assert_eq!(result.expect("ok"), 42);
}

#[tokio::test]
async fn bound_startup_propagates_inner_error() {
    let result = bound_startup(Duration::from_secs(60), async {
        Err::<(), _>(LooprError::DaemonStartup("inner".into()))
    })
    .await;
    let err = result.expect_err("err propagates");
    match err {
        LooprError::DaemonStartup(msg) => assert_eq!(msg, "inner"),
        other => panic!("expected DaemonStartup(\"inner\"), got {other:?}"),
    }
}

#[tokio::test]
async fn bound_startup_returns_daemon_startup_on_elapsed() {
    // Real 50ms budget against a never-resolving future. Plenty of margin
    // under any CI scheduler; the elapsed branch still fires within ~50ms.
    let start = std::time::Instant::now();
    let result = bound_startup::<_, ()>(Duration::from_millis(50), std::future::pending()).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "timeout should fire promptly, took {elapsed:?}"
    );
    let err = result.expect_err("must elapse");
    match err {
        LooprError::DaemonStartup(msg) => {
            // budget.as_secs() truncates 50ms to 0; that's a known edge of
            // the second-granular config field. The point of the test is
            // that the elapsed branch fired and the message format is
            // stable.
            assert!(
                msg.contains("exceeded") && msg.contains("startup budget"),
                "wanted budget breach in error string, got: {msg}"
            );
        }
        other => panic!("expected DaemonStartup, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn drain_pool_logs_panicked_task() {
    // A panicking pipeline task must leave a `warn!` trace at drain time
    // instead of vanishing into the JoinSet (Phase 7: silent death).
    let run_dir = TempDir::new().unwrap();
    {
        let _guard = telemetry::init_for_test(run_dir.path(), "debug").expect("init_for_test");
        let mut tasks: JoinSet<()> = JoinSet::new();
        tasks.spawn(async { panic!("boom in implementer task") });
        // Generous budget: the task panics immediately, so the drain
        // completes well within it (no abort path taken).
        drain_pool(&mut tasks, 5, "implementer").await;
    }
    let events = read_events(run_dir.path());
    assert!(
        has_event_with_pool(&events, "task panicked during shutdown drain", "implementer"),
        "expected a drain-time panic warning tagged pool=implementer; events: {events:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn reap_finished_logs_panicked_task() {
    // The mid-run reaper surfaces a panic without waiting for shutdown.
    let run_dir = TempDir::new().unwrap();
    {
        let _guard = telemetry::init_for_test(run_dir.path(), "debug").expect("init_for_test");
        let mut tasks: JoinSet<()> = JoinSet::new();
        tasks.spawn(async { panic!("boom mid-run") });
        // Let the panicking task complete so `try_join_next` observes it.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        reap_finished(&mut tasks, "director");
    }
    let events = read_events(run_dir.path());
    assert!(
        has_event_with_pool(&events, "task panicked (reaped mid-run)", "director"),
        "expected a reaped panic warning tagged pool=director; events: {events:?}"
    );
}
