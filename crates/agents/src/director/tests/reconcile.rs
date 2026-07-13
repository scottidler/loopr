#![allow(clippy::unwrap_used)]
//! `reconcile_director` sweep tests: goal-complete detection, Integrated->Done
//! promotion, and Phase 3 stuck-state recovery (Triaged-no-Reviewer,
//! Accepted-no-Integrator, InProgress-no-Implementer). Pulled into its own
//! submodule (mirroring `operator.rs` / `restart.rs`) to keep the parent
//! `tests.rs` under the 1500-line bloat-task cap.
//!
//! `use super::*;` imports the scaffolding (`FakeStore`, `RecordingSpawner`,
//! `make_work`, `make_bundle`) from the parent `tests` module via submodule
//! privilege.

use std::sync::Arc;

use domain::{Bundle, BundleStatus, PlanId, Work, WorkId, WorkStatus, now_millis};

use super::{FakeStore, RecordingSpawner, make_bundle, make_work};
use crate::director::reconcile_director;

// ---------------------------------------------------------------------------
// reconcile_director
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_promotes_integrated_work() {
    let plan_id = PlanId::new();
    let integrated = make_work(plan_id.clone(), "wk-integrated", WorkStatus::Integrated);
    let pending = make_work(plan_id.clone(), "wk-pending", WorkStatus::Pending);
    let store = FakeStore::with(vec![integrated.clone(), pending], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let goal_complete = reconcile_director(&plan_id, &store, &spawner, 0).await.expect("ok");
    assert!(!goal_complete, "Pending Work in flight; not GoalComplete");

    let calls = spawner.override_work_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "exactly one Integrated->Done override");
    assert_eq!(calls[0].0, integrated.id);
    assert_eq!(calls[0].1, WorkStatus::Done);
    assert!(calls[0].2.contains("reconcile"));
}

#[tokio::test]
async fn reconcile_goal_complete_when_all_terminal_and_any_done() {
    let plan_id = PlanId::new();
    let done = make_work(plan_id.clone(), "wk-done", WorkStatus::Done);
    let abandoned = make_work(plan_id.clone(), "wk-abandoned", WorkStatus::Abandoned);
    let store = FakeStore::with(vec![done, abandoned], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let goal_complete = reconcile_director(&plan_id, &store, &spawner, 0).await.expect("ok");
    assert!(goal_complete);
    assert!(spawner.override_work_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reconcile_zero_works_returns_false() {
    let plan_id = PlanId::new();
    let store = FakeStore::with(vec![], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let goal_complete = reconcile_director(&plan_id, &store, &spawner, 0).await.expect("ok");
    assert!(!goal_complete, "zero works is not GoalComplete");
}

// ---------------------------------------------------------------------------
// Phase 3 stuck-state recovery: Triaged-no-Reviewer, Accepted-no-Integrator,
// InProgress-no-Implementer. Each case has three permutations:
//   * past grace AND no live task -> recovery fires
//   * within grace -> recovery skipped regardless of live state
//   * past grace AND live task -> recovery skipped (sidecar map covers it)
//
// Grace is `grace_ms` parameter; tests use 30_000 (the default 30s) or 0
// depending on what they want to assert. `updated_at` is mutated post-
// construction; the helpers themselves don't expose it.
// ---------------------------------------------------------------------------

const PAST_GRACE: i64 = 60_000;

fn aged_work(plan_id: PlanId, title: &str, status: WorkStatus, age_ms: i64) -> Work {
    let mut w = make_work(plan_id, title, status);
    w.updated_at = now_millis() - age_ms;
    w
}

fn aged_bundle(work_id: WorkId, status: BundleStatus, age_ms: i64) -> Bundle {
    let mut b = make_bundle(work_id, status);
    b.updated_at = now_millis() - age_ms;
    b
}

#[tokio::test]
async fn reconcile_stuck_triaged_bundle_respawns_reviewer() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Triaged, PAST_GRACE);
    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let spawner = Arc::new(RecordingSpawner::default());
    // live_reviewer_bundle_ids stays empty -> bundle is stuck.

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    let calls = spawner.spawn_reviewer_calls.lock().unwrap();
    assert_eq!(
        *calls,
        vec![bundle.id],
        "stuck Triaged Bundle must trigger spawn_reviewer"
    );
}

#[tokio::test]
async fn reconcile_triaged_bundle_within_grace_window_is_skipped() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    // age=0 -> fresh, well inside any grace window > 0.
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Triaged, 0);
    let store = FakeStore::with(vec![work], vec![bundle]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.spawn_reviewer_calls.lock().unwrap().is_empty(),
        "Triaged Bundle within grace window must NOT trigger spawn_reviewer"
    );
}

#[tokio::test]
async fn reconcile_triaged_bundle_with_live_reviewer_is_skipped() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Triaged, PAST_GRACE);
    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let spawner = Arc::new(RecordingSpawner::default());
    spawner.live_reviewer_bundle_ids.lock().unwrap().push(bundle.id);

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.spawn_reviewer_calls.lock().unwrap().is_empty(),
        "Triaged Bundle WITH a live Reviewer must NOT trigger spawn_reviewer"
    );
}

#[tokio::test]
async fn reconcile_stuck_accepted_bundle_spawns_integrator() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Accepted, PAST_GRACE);
    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    let calls = spawner.spawn_integrator_calls.lock().unwrap();
    assert_eq!(
        *calls,
        vec![bundle.id],
        "stuck Accepted Bundle must trigger spawn_integrator"
    );
}

#[tokio::test]
async fn reconcile_accepted_bundle_within_grace_window_is_skipped() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Accepted, 0);
    let store = FakeStore::with(vec![work], vec![bundle]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.spawn_integrator_calls.lock().unwrap().is_empty(),
        "Accepted Bundle within grace window must NOT trigger spawn_integrator"
    );
}

#[tokio::test]
async fn reconcile_accepted_bundle_with_live_integrator_is_skipped() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Accepted, PAST_GRACE);
    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let spawner = Arc::new(RecordingSpawner::default());
    spawner.live_integrator_bundle_ids.lock().unwrap().push(bundle.id);

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.spawn_integrator_calls.lock().unwrap().is_empty(),
        "Accepted Bundle WITH a live Integrator must NOT trigger spawn_integrator"
    );
}

#[tokio::test]
async fn reconcile_stuck_in_progress_work_recovers_via_reactor_role() {
    let plan_id = PlanId::new();
    let work = aged_work(plan_id.clone(), "wk-1", WorkStatus::InProgress, PAST_GRACE);
    let store = FakeStore::with(vec![work.clone()], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    let calls = spawner.recover_in_progress_calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        1,
        "stuck InProgress Work must trigger one recover_in_progress_work call"
    );
    assert_eq!(calls[0].0, work.id);
    assert!(
        calls[0].1.contains("no live Implementer"),
        "reason must explain the recovery: {}",
        calls[0].1
    );

    // The Director-role `override_work` path must NOT fire for InProgress
    // recovery (the override table is Reactor-only for InProgress -> Ready).
    assert!(
        spawner.override_work_calls.lock().unwrap().is_empty(),
        "InProgress recovery must NOT route through override_work (Director role)"
    );
}

#[tokio::test]
async fn reconcile_in_progress_work_within_grace_window_is_skipped() {
    let plan_id = PlanId::new();
    let work = aged_work(plan_id.clone(), "wk-1", WorkStatus::InProgress, 0);
    let store = FakeStore::with(vec![work], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.recover_in_progress_calls.lock().unwrap().is_empty(),
        "InProgress Work within grace window must NOT trigger recover_in_progress_work"
    );
}

#[tokio::test]
async fn reconcile_in_progress_work_with_live_implementer_is_skipped() {
    let plan_id = PlanId::new();
    let work = aged_work(plan_id.clone(), "wk-1", WorkStatus::InProgress, PAST_GRACE);
    let store = FakeStore::with(vec![work.clone()], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());
    spawner.live_work_ids.lock().unwrap().push(work.id);

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.recover_in_progress_calls.lock().unwrap().is_empty(),
        "InProgress Work WITH a live Implementer must NOT trigger recover_in_progress_work"
    );
}
