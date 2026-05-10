#![allow(clippy::unwrap_used)]

//! Integration tests for `startup_reconcile_directors` (Director Phase 1
//! follow-up, Phase 2 of `docs/design/2026-05-09-director-phase-1-followups.md`).
//!
//! The startup reconcile path is what allows a daemon restart to resume
//! per-Plan Director supervision without manual intervention. Two cases:
//!
//! 1. An `Active` Plan on disk gets a fresh Director task spawned into
//!    `ctx.director_tasks` — this is the cold-boot recovery path.
//! 2. A `Stalled` Plan is skipped — this is what closes the cold-boot
//!    death loop introduced when a Director exhausts its retry budget
//!    (the new `PlanStatus::Stalled` marker; Phase 1).
//!
//! The tests pre-seed `plans.jsonl` via a real `Store` opened on a fresh
//! `TempDir`, close that store, then call `build_context` (which runs
//! reconcile -> startup_reconcile_directors). They assert `director_tasks`
//! count immediately, before any `.await` between the lock and the
//! assertion gives the runtime a chance to schedule the spawned task —
//! `JoinSet::len` counts spawned-but-not-joined tasks regardless of
//! whether they have begun running, so the count is robust either way.

mod common;

use std::path::Path;
use std::sync::Arc;

use domain::{Plan, PlanStatus, Role};
use llm::ScriptedLlm;
use loopr::config::Config;
use loopr::daemon::{DaemonContext, build_context};
use store::Store;
use telemetry::digest::process::ProcessSnapshot;
use telemetry::{ProcessId, SessionId};
use tempfile::TempDir;
use tools::SandboxMode;

use common::init_git_repo;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_boot_respawns_director_for_active_plan() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let target = tempdir.path().to_path_buf();

    // Pre-seed: one Active Plan. `Plan::new` defaults to `Active`.
    {
        let store = Store::open(&target).await.unwrap();
        let plan = Plan::new("active-cold-boot".to_string());
        store.plans().create(plan).await.unwrap();
        store.close().await.unwrap();
    }

    let ctx = build_test_context(&target).await;
    {
        let tasks = ctx.director_tasks.lock().await;
        assert_eq!(
            tasks.len(),
            1,
            "Active Plan must respawn exactly one Director task on cold boot"
        );
    }
    teardown(ctx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_boot_skips_stalled_plan() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let target = tempdir.path().to_path_buf();

    // Pre-seed: one Stalled Plan. The transition is Director-role-only
    // per the FSM table; we apply it here so the on-disk record matches
    // exactly what a retry-budget-exhausted Plan looks like in production.
    {
        let store = Store::open(&target).await.unwrap();
        let mut plan = Plan::new("stalled-cold-boot".to_string());
        plan.transition(PlanStatus::Stalled, Role::Director).unwrap();
        store.plans().create(plan).await.unwrap();
        store.close().await.unwrap();
    }

    let ctx = build_test_context(&target).await;
    {
        let tasks = ctx.director_tasks.lock().await;
        assert_eq!(
            tasks.len(),
            0,
            "Stalled Plan must NOT respawn a Director — that is the cold-boot loop fix"
        );
    }
    teardown(ctx).await;
}

/// Build a `DaemonContext` against a pre-seeded target. Mirrors
/// `common::harness::spawn_test_daemon` minus the IPC listener — these
/// tests only exercise the reconcile path that runs inside `build_context`,
/// so no socket is needed.
async fn build_test_context(target: &Path) -> Arc<DaemonContext<ScriptedLlm>> {
    let session_id = SessionId::parse("20260509-000000").unwrap();
    let process_id = ProcessId::parse("pc-rec001").unwrap();
    let target_slug = "-test-reconcile".to_string();

    let mut config = Config::default();
    config.tools.sandbox = SandboxMode::Off;
    // Tight intervals so any spawned Director iterates quickly during the
    // brief lifetime of this test. Production overrides via .loopr/config.yml.
    config.agents.director.poll_interval_secs = 0;
    config.agents.director.idle_interval_secs = 0;

    let snapshot = Arc::new(std::sync::Mutex::new(ProcessSnapshot::new("test-stub-model")));
    let llm = ScriptedLlm::new();
    // Queue a single Director "done" response in case the Active-Plan
    // case's task is scheduled before teardown — keeps the runtime quiet
    // rather than letting a queue-empty panic surface in the test log.
    llm.queue_free_for(
        "claude-opus-4-7",
        Ok(r#"[{"type":"done","summary":"reconcile-test"}]"#.to_string()),
    );

    build_context(
        target.to_path_buf(),
        session_id,
        target_slug,
        process_id,
        0,
        llm,
        config,
        false,
        snapshot,
    )
    .await
    .unwrap()
}

/// Tear down a context: cancel any spawned Director tasks, then unwrap
/// the Arc and close the store. Mirrors `TestDaemon::shutdown` minus the
/// IPC accept-loop drain.
async fn teardown(ctx: Arc<DaemonContext<ScriptedLlm>>) {
    ctx.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    {
        let mut tasks = ctx.director_tasks.lock().await;
        tasks.shutdown().await;
    }
    if let Ok(owned) = Arc::try_unwrap(ctx) {
        let store_clone = Arc::clone(&owned.store);
        drop(owned);
        if let Ok(store) = Arc::try_unwrap(store_clone) {
            let _ = store.close().await;
        }
    }
}
