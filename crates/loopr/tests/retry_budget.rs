#![allow(clippy::unwrap_used)]

//! Phase 4 (Director Phase 1 follow-ups): Layer 1 + Layer 3 retry-budget
//! tests for `transition_and_persist_work`.
//!
//! - Layer 1: every successful transition into `WorkStatus::Ready`
//!   increments `Work.attempt_count` by 1 (1-based counting). Initial
//!   `Pending -> Ready` dispatch is the first attempt; Director-issued
//!   `Blocked -> Ready` retries each add 1.
//! - Layer 3: a hard cap of `MAX_WORK_ATTEMPTS_HARD_CAP = 100` refuses
//!   the persist when a Work would push past the cap. This is the
//!   defense-in-depth backstop behind the Director's soft cap; it
//!   should never fire in well-behaved deployments.

use domain::{AcceptanceCriteria, Plan, Role, Work, WorkStatus};
use loopr::daemon::context::{MAX_WORK_ATTEMPTS_HARD_CAP, transition_and_persist_work};
use store::Store;
use tempfile::TempDir;

async fn fresh_store() -> (TempDir, Store) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).await.unwrap();
    (tmp, store)
}

async fn seed_plan_and_work(store: &Store) -> Work {
    let plan = Plan::new("retry-budget".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "wk-test".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["does the thing".to_string()]);
    store.works().create(work.clone()).await.unwrap();
    work
}

#[tokio::test]
async fn pending_to_ready_increments_attempt_count_to_one() {
    let (_tmp, store) = fresh_store().await;
    let mut work = seed_plan_and_work(&store).await;
    assert_eq!(work.attempt_count, 0, "fresh Work starts at 0");

    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Ready, Role::Reactor, false)
        .await
        .expect("transition ok");

    assert_eq!(work.attempt_count, 1, "Pending -> Ready must increment to 1 (1-based)");
    let persisted = store.works().get(&work.id).await.unwrap();
    assert_eq!(persisted.attempt_count, 1, "increment must persist to disk");
}

#[tokio::test]
async fn blocked_to_ready_via_override_increments_each_call() {
    let (_tmp, store) = fresh_store().await;
    let mut work = seed_plan_and_work(&store).await;

    // Walk to Blocked: Pending -> Ready (++) -> InProgress -> Blocked.
    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Ready, Role::Reactor, false)
        .await
        .unwrap();
    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::InProgress, Role::Reactor, false)
        .await
        .unwrap();
    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Blocked, Role::Implementer, false)
        .await
        .unwrap();
    assert_eq!(work.attempt_count, 1, "after one Pending->Ready dispatch");

    // Director override: Blocked -> Ready. Cross-iteration retry, ++.
    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Ready, Role::Director, true)
        .await
        .unwrap();
    assert_eq!(work.attempt_count, 2, "second Ready dispatch");

    // Walk back to Blocked and retry again.
    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::InProgress, Role::Reactor, false)
        .await
        .unwrap();
    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Blocked, Role::Implementer, false)
        .await
        .unwrap();
    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Ready, Role::Director, true)
        .await
        .unwrap();
    assert_eq!(work.attempt_count, 3, "third Ready dispatch");
}

#[tokio::test]
async fn unchanged_transition_skips_increment() {
    // Same-state transitions return `Transition::Unchanged` from the
    // FSM and short-circuit before the increment. The counter stays put.
    let (_tmp, store) = fresh_store().await;
    let mut work = seed_plan_and_work(&store).await;

    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Ready, Role::Reactor, false)
        .await
        .unwrap();
    assert_eq!(work.attempt_count, 1);

    // Ready -> Ready: Unchanged result, no increment, no store write.
    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Ready, Role::Reactor, false)
        .await
        .unwrap();
    assert_eq!(work.attempt_count, 1, "Unchanged FSM result must not increment");
}

#[tokio::test]
async fn non_ready_target_skips_increment() {
    // Increment fires only on transitions whose target is Ready.
    // Pending -> Ready bumps once; subsequent Ready -> InProgress does not.
    let (_tmp, store) = fresh_store().await;
    let mut work = seed_plan_and_work(&store).await;

    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Ready, Role::Reactor, false)
        .await
        .unwrap();
    transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::InProgress, Role::Reactor, false)
        .await
        .unwrap();
    assert_eq!(work.attempt_count, 1, "InProgress is not Ready; no increment");
}

#[tokio::test]
async fn hard_cap_refuses_persist_at_max() {
    // attempt_count==HARD_CAP triggers the Layer-3 refusal. The pre-
    // increment check (>=) prevents the (HARD_CAP+1)th attempt from
    // landing; nothing is persisted; the caller surfaces the error.
    let (_tmp, store) = fresh_store().await;
    let mut work = seed_plan_and_work(&store).await;

    // Fast-forward attempt_count to HARD_CAP. The Work needs to be in a
    // state from which `transition` to Ready is valid (Blocked uses
    // Director override; Pending uses Reactor transition).
    work.attempt_count = MAX_WORK_ATTEMPTS_HARD_CAP;
    let err = transition_and_persist_work::<Store>(&store, &mut work, WorkStatus::Ready, Role::Reactor, false)
        .await
        .expect_err("must refuse at hard cap");
    assert!(
        err.contains("MAX_WORK_ATTEMPTS_HARD_CAP"),
        "error must name the cap: {err}"
    );
    assert_eq!(
        work.attempt_count, MAX_WORK_ATTEMPTS_HARD_CAP,
        "counter must NOT increment when the cap refuses the persist"
    );
}
