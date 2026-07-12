#![allow(clippy::unwrap_used)]

//! Integration tests for the Phase 11 deterministic accept gate (panel
//! must-fix #1 of `docs/design/2026-07-11-verified-swarm.md`).
//!
//! The daemon's accept site (`DaemonSpawner::accept_bundle`) refuses
//! `Reviewed -> Accepted` UNLESS the persisted latest `Review` for the Bundle
//! is `Accept` with zero red referenced CheckRuns. The prompt is not the gate;
//! this code path is.
//!
//! Break-to-prove: the same `accept_bundle` call is made twice against the
//! same seeded Reviewed Bundle. With NO Review on record it is refused (the
//! Bundle stays Reviewed, `bundles_accepted` stays 0). After persisting a
//! valid `Accept` Review it succeeds (Bundle leaves Reviewed,
//! `bundles_accepted` == 1). The ONLY difference between the two is the
//! persisted evidence, so the gate is proven load-bearing: without it, the
//! first call WOULD have accepted (that is exactly the pre-Phase-11 behavior
//! `spawner.rs`'s `Reviewed => {}` arm exhibited).

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agents::WorkSpawner;
use domain::{Bundle, BundleId, BundleStatus, CheckRun, Plan, PlanId, Review, Role, TargetKind, Verdict, Work, WorkId};
use llm::ScriptedLlm;
use loopr::config::Config;
use loopr::daemon::build_context;
use loopr::daemon::context::{DaemonContext, DaemonSpawner};
use telemetry::digest::process::ProcessSnapshot;
use telemetry::{ProcessId, SessionId};
use tempfile::TempDir;
use tools::SandboxMode;

use common::init_git_repo;

async fn build_test_context(target: &Path) -> Arc<DaemonContext<ScriptedLlm>> {
    let llm = ScriptedLlm::new();

    let session_id = SessionId::parse("20260712-000000").unwrap();
    let process_id = ProcessId::parse("pc-acg001").unwrap();
    let target_slug = "-test-accept-gate".to_string();

    let mut config = Config::default();
    config.tools.sandbox = SandboxMode::Off;
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

async fn teardown(ctx: Arc<DaemonContext<ScriptedLlm>>) {
    ctx.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    {
        let mut tasks = ctx.work_spawner_tasks.lock().await;
        tasks.shutdown().await;
    }
    {
        let mut tasks = ctx.integrator_tasks.lock().await;
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

/// Persist a Plan + Work + Bundle walked to `Reviewed`, returning the Bundle id.
async fn seed_reviewed_bundle(ctx: &Arc<DaemonContext<ScriptedLlm>>) -> BundleId {
    let plan = Plan::new("accept-gate-test".to_string());
    let plan_id: PlanId = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let work = Work::new(plan_id, "accept-gate-work".to_string());
    let work_id: WorkId = work.id.clone();
    ctx.store.works().create(work).await.unwrap();

    let mut bundle = Bundle::new(work_id, "accept-gate-branch".to_string(), Vec::new());
    let bundle_id = bundle.id.clone();
    ctx.store.bundles().create(bundle.clone()).await.unwrap();

    for (target, role) in [
        (BundleStatus::Triaged, Role::Reactor),
        (BundleStatus::Reviewed, Role::Reviewer),
    ] {
        let expected = bundle.updated_at;
        bundle.transition(target, role).unwrap();
        let new_ts = ctx
            .store
            .bundles()
            .update(bundle.clone(), expected, role, TargetKind::Normal)
            .await
            .unwrap();
        bundle.updated_at = new_ts;
    }
    bundle_id
}

fn accepted_count(ctx: &Arc<DaemonContext<ScriptedLlm>>) -> u32 {
    ctx.snapshot.lock().unwrap().bundles_accepted
}

async fn bundle_status(ctx: &Arc<DaemonContext<ScriptedLlm>>, bundle_id: &BundleId) -> BundleStatus {
    ctx.store.bundles().get(bundle_id).await.unwrap().status
}

/// Poll until `bundles_accepted` reaches `target` or `timeout` elapses.
async fn wait_for_accepted_count(ctx: &Arc<DaemonContext<ScriptedLlm>>, target: u32, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if accepted_count(ctx) >= target {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    accepted_count(ctx) >= target
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reviewed_bundle_without_persisted_review_is_not_accepted() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    let bundle_id = seed_reviewed_bundle(&ctx).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    assert_eq!(accepted_count(&ctx), 0);
    spawner.accept_bundle(bundle_id.clone());

    // Give the shim + inner task ample time to run and REFUSE.
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        accepted_count(&ctx),
        0,
        "no Review on record: the accept gate must refuse; bundles_accepted stays 0"
    );
    assert_eq!(
        bundle_status(&ctx, &bundle_id).await,
        BundleStatus::Reviewed,
        "refused Bundle must remain Reviewed (no illegal accept)"
    );

    teardown(ctx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reviewed_bundle_with_accept_review_is_accepted() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    let bundle_id = seed_reviewed_bundle(&ctx).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    // Persist a valid Accept Review (round 1, zero red checks). THIS is the
    // only difference from the refused case above.
    let review = Review::new(
        bundle_id.clone(),
        1,
        Verdict::Accept {
            summary: "looks good".to_string(),
        },
        "looks good".to_string(),
        Vec::new(),
        Vec::new(),
        "claude-opus-4-8".to_string(),
    );
    ctx.store.reviews().create(review).await.unwrap();

    assert_eq!(accepted_count(&ctx), 0);
    spawner.accept_bundle(bundle_id.clone());

    let accepted = wait_for_accepted_count(&ctx, 1, Duration::from_secs(2)).await;
    assert!(
        accepted,
        "a persisted Accept Review with zero red checks must let the gate accept (bundles_accepted -> 1)"
    );
    // The Bundle left Reviewed (accepted, then possibly advanced by the
    // integrator the accept spawns).
    assert_ne!(
        bundle_status(&ctx, &bundle_id).await,
        BundleStatus::Reviewed,
        "accepted Bundle must leave Reviewed"
    );

    teardown(ctx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reviewed_bundle_with_change_requested_review_is_not_accepted() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    let bundle_id = seed_reviewed_bundle(&ctx).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    // Latest Review is NOT an Accept -> gate refuses even though the Bundle is
    // Reviewed. (A change-requested round on a bundle that reached Reviewed via
    // a prior state is a torn/ambiguous evidence chain; fail closed.)
    let review = Review::new(
        bundle_id.clone(),
        1,
        Verdict::ChangeRequested {
            summary: "fix it".to_string(),
            reasons: vec![domain::ReviewIssue {
                severity: domain::Severity::Error,
                file: "src.rs".to_string(),
                line: None,
                message: "boom".to_string(),
                suggestion: None,
            }],
        },
        "fix it".to_string(),
        vec![domain::ReviewIssue {
            severity: domain::Severity::Error,
            file: "src.rs".to_string(),
            line: None,
            message: "boom".to_string(),
            suggestion: None,
        }],
        Vec::new(),
        "claude-opus-4-8".to_string(),
    );
    ctx.store.reviews().create(review).await.unwrap();

    spawner.accept_bundle(bundle_id.clone());
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        accepted_count(&ctx),
        0,
        "non-Accept latest Review must be refused by the gate"
    );
    assert_eq!(bundle_status(&ctx, &bundle_id).await, BundleStatus::Reviewed);

    teardown(ctx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reviewed_bundle_with_accept_over_red_check_is_not_accepted() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    let bundle_id = seed_reviewed_bundle(&ctx).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    // A red CheckRun referenced by an Accept review -> refuse (defense in depth
    // behind the Phase 10 code gate).
    let red = CheckRun::new(
        bundle_id.clone(),
        WorkId::new(),
        "cargo test".to_string(),
        1,
        "digest".to_string(),
        "excerpt".to_string(),
        Role::Reviewer,
        10,
    );
    let red_id = red.id.clone();
    ctx.store.check_runs().create(red).await.unwrap();

    let review = Review::new(
        bundle_id.clone(),
        1,
        Verdict::Accept {
            summary: "lgtm".to_string(),
        },
        "lgtm".to_string(),
        Vec::new(),
        vec![red_id],
        "claude-opus-4-8".to_string(),
    );
    ctx.store.reviews().create(review).await.unwrap();

    spawner.accept_bundle(bundle_id.clone());
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        accepted_count(&ctx),
        0,
        "an Accept referencing a red CheckRun must be refused by the gate"
    );
    assert_eq!(bundle_status(&ctx, &bundle_id).await, BundleStatus::Reviewed);

    teardown(ctx).await;
}
