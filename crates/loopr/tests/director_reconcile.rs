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
use std::time::{Duration, Instant};

use domain::{AcceptanceCriteria, Plan, PlanStatus, Role, Work, WorkStatus};
use llm::ScriptedLlm;
use loopr::config::Config;
use loopr::daemon::{DaemonContext, build_context};
use serde_json::json;
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

    // Pre-seed: one Active Plan WITH a Work — a mid-flight Plan that was
    // already decomposed. `Plan::new` defaults to `Active`. (A zero-Works
    // Active Plan now re-decomposes instead of respawning a Director; see
    // `cold_boot_redecomposes_active_plan_with_zero_works`.)
    {
        let store = Store::open(&target).await.unwrap();
        let plan = Plan::new("active-cold-boot".to_string());
        let mut work = Work::new(plan.id.clone(), "wk-1".to_string());
        work.acceptance_criteria = AcceptanceCriteria(vec!["assert it works".to_string()]);
        store.plans().create(plan).await.unwrap();
        store.works().create(work).await.unwrap();
        store.close().await.unwrap();
    }

    let ctx = build_test_context(&target).await;
    {
        let tasks = ctx.director_tasks.lock().await;
        assert_eq!(
            tasks.len(),
            1,
            "Active Plan with Works must respawn exactly one Director task on cold boot"
        );
    }
    teardown(ctx).await;
}

/// Bullet 13: an Active Plan with ZERO Works on cold boot was stalled
/// during/before decomposition (a shutdown/drain that skipped
/// `decompose_and_dispatch`, or a crash mid-decompose). The reconcile
/// must re-enter `decompose_and_dispatch` (a `plan_create_tasks` task),
/// NOT spawn a Director to supervise nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_boot_redecomposes_active_plan_with_zero_works() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let target = tempdir.path().to_path_buf();

    {
        let store = Store::open(&target).await.unwrap();
        let plan = Plan::new("active-zero-works".to_string());
        store.plans().create(plan).await.unwrap();
        store.close().await.unwrap();
    }

    // If the re-decompose task is scheduled before teardown, let decompose
    // fail gracefully (two retryable tool errors → DecomposerError, then
    // Plan->Stalled) rather than panic on an empty tool queue.
    let llm = ScriptedLlm::new();
    llm.queue_tool(Err(llm::LlmError::Retryable {
        reason: llm::RetryableReason::Network {
            detail: "test: no decompose".to_string(),
        },
    }));
    llm.queue_tool(Err(llm::LlmError::Retryable {
        reason: llm::RetryableReason::Network {
            detail: "test: no decompose".to_string(),
        },
    }));
    let ctx = build_test_context_with_llm(&target, llm).await;
    {
        let dtasks = ctx.director_tasks.lock().await;
        assert_eq!(
            dtasks.len(),
            0,
            "zero-Works Active Plan must NOT spawn a Director directly"
        );
    }
    {
        let ptasks = ctx.plan_create_tasks.lock().await;
        assert_eq!(
            ptasks.len(),
            1,
            "zero-Works Active Plan must spawn exactly one re-decompose task"
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

/// Convergence test: end-to-end proof that the Phase 1-4 stack closes
/// the cold-boot death loop.
///
/// Setup: a Plan with one Work pre-seeded as Blocked + attempt_count=99.
/// The Director's first iteration receives a single `override_work ->
/// Ready` action; Layer-2's retry-budget cap (default 3, so 99 >= 3)
/// transitions the Plan to Stalled and exits with NeedHelp. We then
/// build a fresh DaemonContext on the same target and assert that the
/// startup_reconcile_directors filter does NOT respawn a Director —
/// this is what would have happened pre-fix and would have looped on
/// every restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn convergence_retry_exhaustion_stalls_plan_and_skips_on_restart() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let target = tempdir.path().to_path_buf();

    // Pre-seed a Plan + a Blocked Work with attempt_count well above
    // the default cap. We bypass the FSM here because this is a fixture,
    // not a state walk; production callers go through
    // `transition_and_persist_work` and don't hand-write `status`.
    let plan_id = {
        let store = store::Store::open(&target).await.unwrap();
        let plan = Plan::new("converge".to_string());
        let plan_id = plan.id.clone();
        store.plans().create(plan.clone()).await.unwrap();

        let mut work = Work::new(plan.id.clone(), "wk-stuck".to_string());
        work.status = WorkStatus::Blocked;
        work.attempt_count = 99;
        work.acceptance_criteria = AcceptanceCriteria(vec!["fix it".to_string()]);
        let work_id_s = work.id.to_string();
        store.works().create(work).await.unwrap();
        store.close().await.unwrap();

        // First context: queue a single OverrideWork->Ready action so the
        // Director's first iteration trips the cap. NeedHelp doesn't
        // restart, so one response is enough — `repeating` would mask a
        // bug where the cap doesn't fire.
        let llm = ScriptedLlm::new();
        llm.queue_free_for(
            "claude-opus-4-7",
            Ok(json!([{
                "action": "override_work",
                "work_id": work_id_s,
                "target_status": "Ready",
                "reason": "retry"
            }])
            .to_string()),
        );

        let ctx = build_test_context_with_llm(&target, llm).await;

        // Poll: the Director runs in a spawned task, so we wait for the
        // Plan -> Stalled persist before tearing down.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let p = ctx.store.plans().get(&plan_id).await.unwrap();
            if p.status == PlanStatus::Stalled {
                break;
            }
            if Instant::now() > deadline {
                panic!("Plan never reached Stalled within 5s (status={:?})", p.status);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        teardown(ctx).await;
        plan_id
    };

    // Second context: a fresh daemon boot against the same target. The
    // Plan is now Stalled on disk; startup_reconcile_directors's filter
    // (`status == PlanStatus::Active`) skips it, and director_tasks
    // stays empty — exactly the cold-boot loop fix.
    let ctx = build_test_context(&target).await;
    {
        let tasks = ctx.director_tasks.lock().await;
        assert_eq!(
            tasks.len(),
            0,
            "after Stalled persist, daemon restart MUST NOT respawn a Director"
        );
    }
    let p = ctx.store.plans().get(&plan_id).await.unwrap();
    assert_eq!(p.status, PlanStatus::Stalled, "Plan must remain Stalled across boots");
    teardown(ctx).await;
}

/// Build a `DaemonContext` against a pre-seeded target. Mirrors
/// `common::harness::spawn_test_daemon` minus the IPC listener — these
/// tests only exercise the reconcile path that runs inside `build_context`,
/// so no socket is needed.
async fn build_test_context(target: &Path) -> Arc<DaemonContext<ScriptedLlm>> {
    let llm = ScriptedLlm::new();
    // Queue a single Director "done" response in case the Active-Plan
    // case's task is scheduled before teardown — keeps the runtime quiet
    // rather than letting a queue-empty panic surface in the test log.
    llm.queue_free_for(
        "claude-opus-4-7",
        Ok(r#"[{"type":"done","summary":"reconcile-test"}]"#.to_string()),
    );
    build_test_context_with_llm(target, llm).await
}

/// Sibling of `build_test_context` that lets a caller pre-seed the
/// `ScriptedLlm` with a specific Director response. Used by the
/// convergence test where a single `override_work` action must trip
/// the Layer-2 retry-budget cap.
async fn build_test_context_with_llm(target: &Path, llm: ScriptedLlm) -> Arc<DaemonContext<ScriptedLlm>> {
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
        let mut tasks = ctx.plan_create_tasks.lock().await;
        tasks.shutdown().await;
    }
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
