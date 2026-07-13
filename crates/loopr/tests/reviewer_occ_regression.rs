#![allow(clippy::unwrap_used)]

//! Regression test for the reviewer OCC self-stale race (Link 1,
//! `docs/design/2026-07-12-reviewer-occ-stale-race.md`, fixed `5dfd3112`;
//! folded into `docs/design/2026-07-12-failure-paths-recovery-chain.md`
//! Phase 3).
//!
//! Drives `DaemonContext::spawn_reviewer_for_bundle` directly against a
//! seeded `Proposed` Bundle with a scripted verdict and asserts the Bundle
//! lands `Reviewed`/`Rejected` on the FIRST round, with exactly one
//! persisted `Review` row. Pre-fix, the daemon's triage step discarded the
//! floored `updated_at` its own write returned, so the reviewer's final
//! `Triaged -> Reviewed`/`Rejected` write lost to a self-inflicted Stale on
//! the very same call — the Bundle stayed Triaged and reconcile re-spawned
//! into a doom loop, appending a fresh Review row every ~34s. This test
//! would have failed (or accumulated >1 Review rows under a slower
//! deadline) against that shape.

mod common;

use std::path::Path;
use std::sync::Arc;

use domain::{Bundle, BundleId, BundleStatus, Plan, PlanId, Work, WorkId, WorkStatus};
use llm::ScriptedLlm;
use loopr::config::Config;
use loopr::daemon::{DaemonContext, build_context};
use telemetry::digest::process::ProcessSnapshot;
use telemetry::{ProcessId, SessionId};
use tempfile::TempDir;
use tools::SandboxMode;

use common::init_git_repo;

async fn build_test_context(target: &Path, llm: ScriptedLlm) -> Arc<DaemonContext<ScriptedLlm>> {
    let session_id = SessionId::parse("20260713-000000").unwrap();
    let process_id = ProcessId::parse("pc-rvocc1").unwrap();
    let target_slug = "-test-reviewer-occ".to_string();

    let mut config = Config::default();
    config.tools.sandbox = SandboxMode::Off;
    config.agents.director.poll_interval_secs = 0;
    config.agents.director.idle_interval_secs = 0;
    // Phase 12 (validation-by-default): this file tests the reviewer's OCC
    // path, not Integrator validation; opt out of the
    // require-validation-by-default startup gate.
    config.integrator.require_validation = false;

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
    if let Ok(owned) = Arc::try_unwrap(ctx) {
        let store_clone = Arc::clone(&owned.store);
        drop(owned);
        if let Ok(store) = Arc::try_unwrap(store_clone) {
            let _ = store.close().await;
        }
    }
}

/// Persist a Plan + Work (`InReview` fixture status, matching the daemon's
/// real pre-review state) + a `Proposed` Bundle. The Work FSM is bypassed
/// here as a fixture, not a state walk — same precedent as
/// `director_reconcile.rs`'s pre-seeded Blocked Work.
///
/// The Bundle's `updated_at`/`created_at` are forced into the future so the
/// daemon's own triage write (step 1 of `spawn_reviewer_for_bundle`)
/// deterministically floors to `current.updated_at + 1` — the same-
/// millisecond OCC window that doomed the reviewer (`docs/design/2026-07-12-
/// reviewer-occ-stale-race.md`) — regardless of how much real wall-clock
/// time the daemon's own startup (git init, `build_context`) consumes
/// before triage runs. Same idiom as the store-seam test in
/// `crates/store/src/bundles/tests.rs`.
async fn seed_proposed_bundle(ctx: &Arc<DaemonContext<ScriptedLlm>>) -> (WorkId, BundleId) {
    let plan = Plan::new("reviewer-occ-test".to_string());
    let plan_id: PlanId = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let mut work = Work::new(plan_id, "reviewer-occ-work".to_string());
    work.status = WorkStatus::InReview;
    let work_id: WorkId = work.id.clone();
    ctx.store.works().create(work).await.unwrap();

    let mut bundle = Bundle::new(
        work_id.clone(),
        "loopr/reviewer-occ".to_string(),
        vec!["noop claim".to_string()],
    );
    let future = domain::now_millis() + 1_000_000;
    bundle.updated_at = future;
    bundle.created_at = future;
    let bundle_id = bundle.id.clone();
    ctx.store.bundles().create(bundle).await.unwrap();
    (work_id, bundle_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_reviewer_for_bundle_lands_reviewed_with_one_review_row() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());

    let llm = ScriptedLlm::new();
    llm.queue_free(Ok(r#"{"kind":"accept","summary":"looks fine"}"#.to_string()));

    let ctx = build_test_context(tempdir.path(), llm).await;
    let (_work_id, bundle_id) = seed_proposed_bundle(&ctx).await;
    let bundle = ctx.store.bundles().get(&bundle_id).await.unwrap();

    Arc::clone(&ctx).spawn_reviewer_for_bundle(bundle).await;

    let after = ctx.store.bundles().get(&bundle_id).await.unwrap();
    assert_eq!(
        after.status,
        BundleStatus::Reviewed,
        "an Accept verdict on the first round must land Reviewed \
         (pre-fix: self-stale doom loop, stuck at Triaged)"
    );

    let reviews = ctx.store.reviews().list_by_bundle(&bundle_id).await.unwrap();
    assert_eq!(
        reviews.len(),
        1,
        "exactly one Review row for the single review round; \
         the pre-fix doom loop appended one per re-spawn"
    );

    teardown(ctx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_reviewer_for_bundle_lands_rejected_with_one_review_row() {
    let tempdir = TempDir::new().unwrap();
    init_git_repo(tempdir.path());

    let llm = ScriptedLlm::new();
    llm.queue_free(Ok(r#"{"kind":"reject","reason":"scripted rejection"}"#.to_string()));

    let ctx = build_test_context(tempdir.path(), llm).await;
    let (_work_id, bundle_id) = seed_proposed_bundle(&ctx).await;
    let bundle = ctx.store.bundles().get(&bundle_id).await.unwrap();

    Arc::clone(&ctx).spawn_reviewer_for_bundle(bundle).await;

    let after = ctx.store.bundles().get(&bundle_id).await.unwrap();
    assert_eq!(
        after.status,
        BundleStatus::Rejected,
        "a Reject verdict must land Rejected"
    );

    let reviews = ctx.store.reviews().list_by_bundle(&bundle_id).await.unwrap();
    assert_eq!(reviews.len(), 1, "exactly one Review row for the single review round");

    teardown(ctx).await;
}
