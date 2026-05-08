#![allow(clippy::unwrap_used)]

//! Integration tests for the dependency gate.
//!
//! These tests verify the dep-gate logic at the domain + store boundary:
//! partitioning at dispatch time (Phase 2) and the pure dep-resolution
//! helpers (Phase 1). Full end-to-end with a live daemon is covered by
//! the process-level harness in `stage_7_wiring.rs` and
//! `stage_8_plan_to_tick.rs`; these tests focus on the dep gate's own
//! invariants.

use domain::{Plan, PlanId, Work, WorkId, WorkStatus};
use tempfile::TempDir;

async fn fresh_store() -> (TempDir, store::Store) {
    let dir = TempDir::new().unwrap();
    let s = store::Store::open(dir.path()).await.unwrap();
    (dir, s)
}

fn work_with_deps(plan_id: &PlanId, title: &str, deps: Vec<WorkId>) -> Work {
    let mut w = Work::new(plan_id.clone(), title.to_string());
    w.dependencies = deps;
    w
}

fn make_done(plan_id: &PlanId, title: &str) -> Work {
    let mut w = Work::new(plan_id.clone(), title.to_string());
    w.status = WorkStatus::Done;
    w
}

// ---------------------------------------------------------------------------
// all_deps_done partition mirrors Phase 2 handler.rs logic
// ---------------------------------------------------------------------------

#[test]
fn partition_no_dep_works_are_all_unblocked() {
    let plan_id = Plan::new("goal".to_string()).id;
    let w1 = Work::new(plan_id.clone(), "a".to_string());
    let w2 = Work::new(plan_id.clone(), "b".to_string());
    let works = vec![w1, w2];
    let (unblocked, held): (Vec<_>, Vec<_>) = works.iter().partition(|w| w.all_deps_done(&works));
    assert_eq!(unblocked.len(), 2);
    assert_eq!(held.len(), 0);
}

#[test]
fn partition_dep_works_are_held() {
    let plan_id = Plan::new("goal".to_string()).id;
    let w_a = Work::new(plan_id.clone(), "a".to_string());
    let w_b = work_with_deps(&plan_id, "b", vec![w_a.id.clone()]);
    let w_c = work_with_deps(&plan_id, "c", vec![w_b.id.clone()]);
    let works = vec![w_a.clone(), w_b.clone(), w_c.clone()];
    let (unblocked, held): (Vec<_>, Vec<_>) = works.iter().partition(|w| w.all_deps_done(&works));
    // Only A has no deps - B and C are held
    assert_eq!(unblocked.len(), 1);
    assert_eq!(unblocked[0].id, w_a.id);
    assert_eq!(held.len(), 2);
}

#[test]
fn partition_done_dep_unblocks_dependent() {
    let plan_id = Plan::new("goal".to_string()).id;
    let w_a = make_done(&plan_id, "a");
    let w_b = work_with_deps(&plan_id, "b", vec![w_a.id.clone()]);
    let works = vec![w_a.clone(), w_b.clone()];
    let (unblocked, held): (Vec<_>, Vec<_>) = works.iter().partition(|w| w.all_deps_done(&works));
    // A is Done - but we're partitioning ALL works, so A is unblocked (no deps)
    // B's dep A is Done, so B is also unblocked
    assert_eq!(unblocked.len(), 2, "both A and B should be unblocked when A is Done");
    assert_eq!(held.len(), 0);
}

#[test]
fn partition_unknown_dep_id_holds_work() {
    let plan_id = Plan::new("goal".to_string()).id;
    let ghost_id = Work::new(plan_id.clone(), "ghost".to_string()).id;
    let w = work_with_deps(&plan_id, "w", vec![ghost_id]);
    let works = vec![w.clone()];
    let (unblocked, held): (Vec<_>, Vec<_>) = works.iter().partition(|w| w.all_deps_done(&works));
    assert_eq!(unblocked.len(), 0);
    assert_eq!(held.len(), 1, "unknown dep id must hold the Work");
}

// ---------------------------------------------------------------------------
// any_dep_irrecoverable
// ---------------------------------------------------------------------------

#[test]
fn dep_blocked_is_not_irrecoverable() {
    let plan_id = Plan::new("g".to_string()).id;
    let mut dep = Work::new(plan_id.clone(), "dep".to_string());
    dep.status = WorkStatus::Blocked;
    let mut w = Work::new(plan_id.clone(), "w".to_string());
    w.dependencies = vec![dep.id.clone()];
    assert!(
        w.any_dep_irrecoverable(&[dep]).is_none(),
        "Blocked dep must not be irrecoverable"
    );
}

#[test]
fn dep_abandoned_is_irrecoverable() {
    let plan_id = Plan::new("g".to_string()).id;
    let mut dep = Work::new(plan_id.clone(), "dep".to_string());
    dep.status = WorkStatus::Abandoned;
    let dep_id = dep.id.clone();
    let mut w = Work::new(plan_id.clone(), "w".to_string());
    w.dependencies = vec![dep_id.clone()];
    assert_eq!(w.any_dep_irrecoverable(&[dep]), Some(&dep_id));
}

#[test]
fn dep_superseded_is_irrecoverable() {
    let plan_id = Plan::new("g".to_string()).id;
    let mut dep = Work::new(plan_id.clone(), "dep".to_string());
    dep.status = WorkStatus::Superseded;
    let dep_id = dep.id.clone();
    let mut w = Work::new(plan_id.clone(), "w".to_string());
    w.dependencies = vec![dep_id.clone()];
    assert_eq!(w.any_dep_irrecoverable(&[dep]), Some(&dep_id));
}

// ---------------------------------------------------------------------------
// blocked_reason field round-trips through the store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blocked_reason_persists_through_store_round_trip() {
    let (_dir, store) = fresh_store().await;
    let plan = Plan::new("goal".to_string());
    store.plans().create(plan.clone()).await.unwrap();

    let mut work = Work::new(plan.id.clone(), "w".to_string());
    store.works().create(work.clone()).await.unwrap();

    let expected_updated_at = work.updated_at;
    work.blocked_reason = Some("dep x reached Abandoned".to_string());
    work.status = WorkStatus::Blocked;
    // Manually bump updated_at to simulate FSM transition
    work.updated_at += 1;
    store.works().update(work.clone(), expected_updated_at).await.unwrap();

    let fetched = store.works().get(&work.id).await.unwrap();
    assert_eq!(fetched.blocked_reason.as_deref(), Some("dep x reached Abandoned"));
    assert_eq!(fetched.status, WorkStatus::Blocked);
}

// ---------------------------------------------------------------------------
// WorksStore OCC rejects concurrent writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn works_store_occ_rejects_stale_write() {
    let (_dir, store) = fresh_store().await;
    let plan = Plan::new("goal".to_string());
    store.plans().create(plan.clone()).await.unwrap();

    let mut work = Work::new(plan.id.clone(), "w".to_string());
    let original_updated_at = work.updated_at;
    store.works().create(work.clone()).await.unwrap();

    // First write succeeds.
    let mut first = work.clone();
    first.updated_at = original_updated_at + 1;
    first.status = WorkStatus::Ready;
    store.works().update(first, original_updated_at).await.unwrap();

    // Second write with stale expected_updated_at must be rejected.
    work.updated_at = original_updated_at + 2;
    work.status = WorkStatus::InProgress;
    let result = store.works().update(work, original_updated_at).await;
    assert!(
        matches!(result, Err(store::StoreError::Stale { .. })),
        "concurrent write with stale expected_updated_at must return Stale"
    );
}
