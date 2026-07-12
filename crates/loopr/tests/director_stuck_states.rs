#![allow(clippy::unwrap_used)]

//! Integration tests for Director Phase 2 — `WorkSpawner` stuck-state
//! recovery surface (Phase 2 of `docs/design/2026-05-09-director-phase-2.md`).
//!
//! These tests exercise the new `DaemonSpawner` methods directly:
//!
//! - `spawn_reviewer` skips Bundles past Triaged (Reviewed/Accepted/etc.)
//!   and fires for Proposed/Triaged.
//! - `spawn_integrator` skips Bundles not currently Accepted.
//! - `list_running_*_ids` reflect the sidecar-map state populated by
//!   the spawn-task wrappers' `ScopedIdGuard` RAII.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agents::{WorkSpawner, reconcile_director};
use domain::{Bundle, BundleId, BundleStatus, Plan, PlanId, Role, TargetKind, Work, WorkId, WorkStatus};
use llm::ScriptedLlm;
use loopr::config::Config;
use loopr::daemon::build_context;
use loopr::daemon::context::{DaemonContext, DaemonSpawner, ScopedIdGuard};
use telemetry::digest::process::ProcessSnapshot;
use telemetry::{ProcessId, SessionId};
use tempfile::TempDir;
use tools::SandboxMode;

use common::init_git_repo;

// ---------- shared scaffolding ----------

async fn build_test_context(target: &Path) -> Arc<DaemonContext<ScriptedLlm>> {
    let llm = ScriptedLlm::new();
    llm.queue_free_for(
        "claude-opus-4-7",
        Ok(r#"[{"type":"done","summary":"phase-2-stuck-states"}]"#.to_string()),
    );

    let session_id = SessionId::parse("20260510-000000").unwrap();
    let process_id = ProcessId::parse("pc-stk001").unwrap();
    let target_slug = "-test-stuck-states".to_string();

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
        let mut tasks = ctx.director_tasks.lock().await;
        tasks.shutdown().await;
    }
    {
        let mut tasks = ctx.work_spawner_tasks.lock().await;
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

/// Persist a Plan + Work + Bundle, returning the Bundle's ID.
async fn seed_bundle_at_status(ctx: &Arc<DaemonContext<ScriptedLlm>>, target_status: BundleStatus) -> BundleId {
    let plan = Plan::new("phase-2-test".to_string());
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let work = Work::new(plan_id.clone(), "phase-2-work".to_string());
    let work_id = work.id.clone();
    ctx.store.works().create(work).await.unwrap();

    // Build Bundle and walk it to the target status via the FSM.
    let mut bundle = Bundle::new(work_id, "phase-2-branch".to_string(), Vec::new());
    let bundle_id = bundle.id.clone();
    ctx.store.bundles().create(bundle.clone()).await.unwrap();

    // Walk the FSM to target_status. Each transition is OCC-checked.
    let path: &[(BundleStatus, Role)] = match target_status {
        BundleStatus::Proposed => &[],
        BundleStatus::Triaged => &[(BundleStatus::Triaged, Role::Reactor)],
        BundleStatus::Reviewed => &[
            (BundleStatus::Triaged, Role::Reactor),
            (BundleStatus::Reviewed, Role::Reviewer),
        ],
        BundleStatus::Accepted => &[
            (BundleStatus::Triaged, Role::Reactor),
            (BundleStatus::Reviewed, Role::Reviewer),
            (BundleStatus::Accepted, Role::Director),
        ],
        BundleStatus::Integrating => &[
            (BundleStatus::Triaged, Role::Reactor),
            (BundleStatus::Reviewed, Role::Reviewer),
            (BundleStatus::Accepted, Role::Director),
            (BundleStatus::Integrating, Role::Integrator),
        ],
        other => panic!("seed helper does not yet handle BundleStatus::{other:?}"),
    };
    for (target, role) in path {
        let expected = bundle.updated_at;
        bundle.transition(*target, *role).unwrap();
        // Thread the persisted (monotonically-floored) updated_at back into
        // the in-memory bundle so the next chained transition's OCC
        // expected-version matches even when both writes land in the same
        // millisecond (the F2 floor makes them strictly increasing).
        let new_ts = ctx
            .store
            .bundles()
            .update(bundle.clone(), expected, *role, TargetKind::Normal)
            .await
            .unwrap();
        bundle.updated_at = new_ts;
    }
    bundle_id
}

/// Poll `pool` until `pool.lock().await.len() > baseline` or `timeout`
/// elapses. Returns `true` on growth, `false` on timeout. Used after
/// fire-and-forget spawn calls whose effects ride two layers of
/// tokio::spawn (sync-trait shim + inner task body).
async fn wait_for_pool_growth(
    pool: &tokio::sync::Mutex<tokio::task::JoinSet<()>>,
    baseline: usize,
    timeout: Duration,
) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if pool.lock().await.len() > baseline {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pool.lock().await.len() > baseline
}

// ---------- list_running_*_ids tests ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_running_work_ids_reflects_sidecar_map() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    // Empty initially.
    assert!(spawner.list_running_work_ids().is_empty());

    // Insert two via the same RAII helper the spawn wrappers use.
    let k1 = WorkId::new();
    let k2 = WorkId::new();
    let g1 = ScopedIdGuard::new(Arc::clone(&ctx.implementer_work_ids), k1.clone());
    let g2 = ScopedIdGuard::new(Arc::clone(&ctx.implementer_work_ids), k2.clone());

    let listed = spawner.list_running_work_ids();
    assert_eq!(listed.len(), 2);
    assert!(listed.contains(&k1));
    assert!(listed.contains(&k2));

    drop(g1);
    let listed = spawner.list_running_work_ids();
    assert_eq!(listed.len(), 1);
    assert!(listed.contains(&k2));

    drop(g2);
    assert!(spawner.list_running_work_ids().is_empty());

    teardown(ctx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_running_reviewer_and_integrator_bundle_ids_reflect_sidecar_maps() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    let bid_r = BundleId::new();
    let bid_i = BundleId::new();

    let _gr = ScopedIdGuard::new(Arc::clone(&ctx.reviewer_bundle_ids), bid_r.clone());
    let _gi = ScopedIdGuard::new(Arc::clone(&ctx.integrator_bundle_ids), bid_i.clone());

    // Each list helper shows ONLY its own pool — no cross-contamination.
    let r_list = spawner.list_running_reviewer_bundle_ids();
    let i_list = spawner.list_running_integrator_bundle_ids();
    assert_eq!(r_list, vec![bid_r.clone()]);
    assert_eq!(i_list, vec![bid_i.clone()]);

    teardown(ctx).await;
}

// ---------- spawn_reviewer status-filter tests ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_reviewer_skips_already_reviewed_bundle() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    let bundle_id = seed_bundle_at_status(&ctx, BundleStatus::Reviewed).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    let reviewer_pool_before = ctx.reviewer_tasks.lock().await.len();
    spawner.spawn_reviewer(bundle_id);

    // Give the shim + inner task time to run and skip.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let reviewer_pool_after = ctx.reviewer_tasks.lock().await.len();
    assert_eq!(
        reviewer_pool_after, reviewer_pool_before,
        "Reviewer pool must NOT grow for an already-Reviewed Bundle (the spawn must skip)"
    );

    teardown(ctx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_reviewer_fires_for_triaged_bundle() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    let bundle_id = seed_bundle_at_status(&ctx, BundleStatus::Triaged).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    let reviewer_pool_before = ctx.reviewer_tasks.lock().await.len();
    spawner.spawn_reviewer(bundle_id);

    // Wait until the new Reviewer task lands in the pool.
    let observed = wait_for_pool_growth(&ctx.reviewer_tasks, reviewer_pool_before, Duration::from_secs(2)).await;
    assert!(
        observed,
        "Reviewer pool must grow for a Triaged Bundle (spawn_reviewer must fire)"
    );

    teardown(ctx).await;
}

// ---------- spawn_integrator status-filter tests ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_integrator_skips_non_accepted_bundle() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    // Reviewed is the most likely "should not fire integrator" state -
    // accept_bundle is the only path to Accepted, and the design's
    // contract is that spawn_integrator runs only on Accepted.
    let bundle_id = seed_bundle_at_status(&ctx, BundleStatus::Reviewed).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    let integrator_pool_before = ctx.integrator_tasks.lock().await.len();
    spawner.spawn_integrator(bundle_id);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let integrator_pool_after = ctx.integrator_tasks.lock().await.len();
    assert_eq!(
        integrator_pool_after, integrator_pool_before,
        "Integrator pool must NOT grow for a non-Accepted Bundle (the spawn must skip)"
    );

    teardown(ctx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_integrator_fires_for_accepted_bundle() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;
    let bundle_id = seed_bundle_at_status(&ctx, BundleStatus::Accepted).await;
    let spawner = DaemonSpawner(Arc::clone(&ctx));

    let integrator_pool_before = ctx.integrator_tasks.lock().await.len();
    spawner.spawn_integrator(bundle_id);

    let observed = wait_for_pool_growth(&ctx.integrator_tasks, integrator_pool_before, Duration::from_secs(2)).await;
    assert!(
        observed,
        "Integrator pool must grow for an Accepted Bundle (spawn_integrator must fire)"
    );

    teardown(ctx).await;
}

// ---------- Phase 3 end-to-end: reconcile sweep -> stuck-state recovery ----------

/// Phase 3 of `docs/design/2026-05-09-director-phase-2.md`: end-to-end
/// proof that the reconcile sweep detects a stuck `InProgress` Work
/// (no live Implementer in the sidecar map) and recovers it via
/// `recover_in_progress_work`, which routes through
/// `transition_and_persist_work` under `Role::Reactor` and lands the
/// Work in `Ready` with `attempt_count` bumped (Layer-1 increment).
///
/// We seed a Work straight into `InProgress` without ever spawning an
/// Implementer, so `implementer_work_ids` stays empty -> the
/// `list_running_work_ids` snapshot does not contain this Work's id.
/// `grace_ms=0` is used so any age qualifies; the production grace of
/// 30s would force a real-time wait or `updated_at` mutation, both
/// avoidable here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_recovers_in_progress_work_with_no_live_implementer() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;

    // Seed: Plan + Work walked to InProgress via the FSM.
    let plan = Plan::new("phase-3-stuck-work".to_string());
    let plan_id: PlanId = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let mut work = Work::new(plan_id.clone(), "phase-3-stuck".to_string());
    let work_id: WorkId = work.id.clone();
    ctx.store.works().create(work.clone()).await.unwrap();

    // Walk Pending -> Ready -> InProgress under Role::Reactor via direct
    // store update. This path bypasses `transition_and_persist_work`'s
    // Layer-1 attempt_count increment by design (the daemon helper is the
    // only writer that bumps); seeding directly leaves attempt_count at 0,
    // which is the cleanest baseline for asserting the recovery path's
    // increment.
    for target in [WorkStatus::Ready, WorkStatus::InProgress] {
        let expected = work.updated_at;
        work.transition(target, Role::Reactor).unwrap();
        ctx.store
            .works()
            .update(work.clone(), expected, Role::Reactor, TargetKind::Normal)
            .await
            .unwrap();
        work = ctx.store.works().get(&work_id).await.unwrap();
    }
    assert_eq!(work.status, WorkStatus::InProgress);
    let attempt_at_inprogress = work.attempt_count;

    // Run the reconcile sweep. `grace_ms = 0` so the InProgress record
    // (created milliseconds ago) qualifies as past-grace.
    let spawner = DaemonSpawner(Arc::clone(&ctx));
    let goal_complete = reconcile_director(&plan_id, &ctx.store, &spawner, 0)
        .await
        .expect("reconcile_director must succeed");
    assert!(!goal_complete, "Work is not Done; Plan cannot be GoalComplete");

    // The recovery is fire-and-forget through two layers of tokio::spawn;
    // poll until the Work flips to Ready (or the budget elapses).
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut observed_ready = false;
    while Instant::now() < deadline {
        let w = ctx.store.works().get(&work_id).await.unwrap();
        if w.status == WorkStatus::Ready {
            observed_ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(observed_ready, "Work must transition to Ready after reconcile recovery");

    let final_work = ctx.store.works().get(&work_id).await.unwrap();
    assert_eq!(final_work.status, WorkStatus::Ready);
    assert!(
        final_work.attempt_count > attempt_at_inprogress,
        "recover_in_progress_work must trigger Layer-1 attempt_count bump on the new Ready transition (was {}, now {})",
        attempt_at_inprogress,
        final_work.attempt_count
    );

    teardown(ctx).await;
}

/// Phase 3: reconcile must NOT recover an `InProgress` Work whose id is
/// in the live-implementer sidecar map. The `ScopedIdGuard` RAII helper
/// is the production insertion path; we hold one for the duration of
/// the test to simulate "the Implementer is still alive."
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_skips_in_progress_work_with_live_implementer_sidecar() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());
    let ctx = build_test_context(tempdir.path()).await;

    let plan = Plan::new("phase-3-live-implementer".to_string());
    let plan_id: PlanId = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let mut work = Work::new(plan_id.clone(), "phase-3-live".to_string());
    let work_id: WorkId = work.id.clone();
    ctx.store.works().create(work.clone()).await.unwrap();
    for target in [WorkStatus::Ready, WorkStatus::InProgress] {
        let expected = work.updated_at;
        work.transition(target, Role::Reactor).unwrap();
        ctx.store
            .works()
            .update(work.clone(), expected, Role::Reactor, TargetKind::Normal)
            .await
            .unwrap();
        work = ctx.store.works().get(&work_id).await.unwrap();
    }
    let attempt_at_inprogress = work.attempt_count;

    // Hold the sidecar guard so the live snapshot contains this work_id.
    let _guard = ScopedIdGuard::new(Arc::clone(&ctx.implementer_work_ids), work_id.clone());

    let spawner = DaemonSpawner(Arc::clone(&ctx));
    let _ = reconcile_director(&plan_id, &ctx.store, &spawner, 0)
        .await
        .expect("reconcile_director must succeed");

    // Brief window for any (incorrect) spawn-chain to land.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let final_work = ctx.store.works().get(&work_id).await.unwrap();
    assert_eq!(
        final_work.status,
        WorkStatus::InProgress,
        "InProgress Work with a live Implementer in the sidecar map must NOT be recovered"
    );
    assert_eq!(
        final_work.attempt_count, attempt_at_inprogress,
        "attempt_count must not bump when reconcile skips this record"
    );

    drop(_guard);
    teardown(ctx).await;
}
