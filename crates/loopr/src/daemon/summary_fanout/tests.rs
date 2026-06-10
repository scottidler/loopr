#![allow(clippy::unwrap_used)]

//! Unit tests for `SummaryFanout`. The decorator pattern means
//! we exercise three contracts:
//!
//! 1. Each successful inner update produces the matching summary file
//!    on disk.
//! 2. The c-extended Work-update path additionally writes the parent
//!    Plan's summary when the parent resolves.
//! 3. A Work whose parent is missing (or not a Plan) emits `debug!`
//!    and skips silently — no `warn!`.
//!
//! Tests use the real `Store` rather than mocked sink fakes because
//! Phase 5's c-extended path needs `store.plans().get` and
//! `store.works().list_by_parent_id` to actually return data; the
//! whole point of the design is that the decorator does those reads
//! itself. A pure mock-sink test would not exercise the c-extended
//! path's wiring.

use std::sync::Arc;

use tempfile::TempDir;

use domain::{Bundle, Plan, PlanStatus, Work, WorkStatus};
use store::Store;
use store::{BundleUpdateSink, PlanUpdateSink, WorkUpdateSink};

use super::SummaryFanout;

async fn fresh_store() -> (TempDir, Arc<Store>) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    (dir, Arc::new(store))
}

fn summary_path(target: &std::path::Path, kind: &str, id: &str) -> std::path::PathBuf {
    target
        .join(".loopr")
        .join("records")
        .join(kind)
        .join(id)
        .join("summary.md")
}

#[tokio::test]
async fn work_update_writes_work_summary_and_refreshes_parent_plan() {
    let (dir, store) = fresh_store().await;
    let target = dir.path().to_path_buf();

    let plan = Plan::new("parent goal".to_string());
    store.plans().create(plan.clone()).await.expect("create plan");
    let work = Work::new(plan.id.clone(), "the work".to_string());
    store.works().create(work.clone()).await.expect("create work");

    let fanout = SummaryFanout::new(Arc::clone(&store), target.clone(), Arc::clone(&store));
    WorkUpdateSink::update(&fanout, work.clone(), work.updated_at)
        .await
        .expect("update");

    let work_summary = summary_path(&target, "works", work.id.as_ref());
    assert!(
        work_summary.exists(),
        "work summary should exist at {}",
        work_summary.display()
    );

    let plan_summary = summary_path(&target, "plans", plan.id.as_ref());
    assert!(
        plan_summary.exists(),
        "c-extended: plan summary should exist at {}",
        plan_summary.display()
    );

    let plan_body = std::fs::read_to_string(&plan_summary).expect("read plan summary");
    assert!(
        plan_body.contains("the work"),
        "plan summary should reference the child work: {plan_body}"
    );
}

#[tokio::test]
async fn work_update_with_missing_parent_writes_only_work_summary() {
    let (dir, store) = fresh_store().await;
    let target = dir.path().to_path_buf();

    // Construct a Work whose parent_id points at a Plan we never
    // create. The c-extended path's parent-Plan resolve is best-effort:
    // missing-parent emits debug! and skips.
    let bogus_plan = Plan::new("never persisted".to_string());
    let work = Work::new(bogus_plan.id.clone(), "orphaned work".to_string());
    store.works().create(work.clone()).await.expect("create work");

    let fanout = SummaryFanout::new(Arc::clone(&store), target.clone(), Arc::clone(&store));
    WorkUpdateSink::update(&fanout, work.clone(), work.updated_at)
        .await
        .expect("update");

    let work_summary = summary_path(&target, "works", work.id.as_ref());
    assert!(work_summary.exists());

    let plan_summary = summary_path(&target, "plans", bogus_plan.id.as_ref());
    assert!(
        !plan_summary.exists(),
        "missing-parent must not synthesize a Plan summary"
    );
}

#[tokio::test]
async fn bundle_update_writes_bundle_summary() {
    let (dir, store) = fresh_store().await;
    let target = dir.path().to_path_buf();

    let plan = Plan::new("p".to_string());
    store.plans().create(plan.clone()).await.expect("create plan");
    let work = Work::new(plan.id.clone(), "w".to_string());
    store.works().create(work.clone()).await.expect("create work");
    let bundle = Bundle::new(work.id.clone(), "wk-abc/1".to_string(), vec!["it works".to_string()]);
    let expected_updated_at = bundle.updated_at;
    store.bundles().create(bundle.clone()).await.expect("create bundle");

    let fanout = SummaryFanout::new(Arc::clone(&store), target.clone(), Arc::clone(&store));
    BundleUpdateSink::update(&fanout, bundle.clone(), expected_updated_at)
        .await
        .expect("update");

    let bundle_summary = summary_path(&target, "bundles", bundle.id.as_ref());
    assert!(bundle_summary.exists(), "bundle summary should exist");
}

#[tokio::test]
async fn plan_update_writes_plan_summary_and_passes_children_through() {
    let (dir, store) = fresh_store().await;
    let target = dir.path().to_path_buf();

    let plan = Plan::new("plan goal".to_string());
    store.plans().create(plan.clone()).await.expect("create plan");
    let w1 = Work::new(plan.id.clone(), "child a".to_string());
    let w2 = Work::new(plan.id.clone(), "child b".to_string());
    store.works().create(w1.clone()).await.expect("create w1");
    store.works().create(w2.clone()).await.expect("create w2");

    let fanout = SummaryFanout::new(Arc::clone(&store), target.clone(), Arc::clone(&store));
    // Disambiguate: SummaryFanout impls all three sink traits, so a
    // bare `.update()` is ambiguous on the call site. Use the trait-
    // qualified form to lock onto PlanUpdateSink.
    PlanUpdateSink::update(&fanout, plan.clone(), vec![w1.clone(), w2.clone()], plan.updated_at)
        .await
        .expect("update");

    let plan_summary = summary_path(&target, "plans", plan.id.as_ref());
    assert!(plan_summary.exists());
    let body = std::fs::read_to_string(&plan_summary).unwrap();
    assert!(body.contains("child a"));
    assert!(body.contains("child b"));
}

#[tokio::test]
async fn blocked_work_emits_daemon_event_but_non_terminal_does_not() {
    let (dir, store) = fresh_store().await;
    let target = dir.path().to_path_buf();
    let plan = Plan::new("p".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "w".to_string());
    store.works().create(work.clone()).await.unwrap();

    let (events, mut rx) = tokio::sync::broadcast::channel(8);
    let fanout = SummaryFanout::with_events(Arc::clone(&store), target.clone(), Arc::clone(&store), events);

    // A non-terminal transition (Ready) must NOT emit an event.
    work.status = WorkStatus::Ready;
    let persisted = WorkUpdateSink::update(&fanout, work.clone(), work.updated_at)
        .await
        .unwrap();
    work.updated_at = persisted;
    assert!(rx.try_recv().is_err(), "Ready transition must not emit a lifecycle event");

    // A Blocked transition emits `work.blocked` carrying the ids.
    work.status = WorkStatus::Blocked;
    work.blocked_reason = Some("dep failed".to_string());
    WorkUpdateSink::update(&fanout, work.clone(), work.updated_at)
        .await
        .unwrap();
    let event = rx.try_recv().expect("work.blocked event emitted");
    assert_eq!(event.event, "work.blocked");
    assert_eq!(event.data["work_id"], work.id.to_string());
    assert_eq!(event.data["plan_id"], plan.id.to_string());
    assert_eq!(event.data["blocked_reason"], "dep failed");
}

#[tokio::test]
async fn stalled_plan_emits_daemon_event() {
    let (dir, store) = fresh_store().await;
    let target = dir.path().to_path_buf();
    let mut plan = Plan::new("p".to_string());
    store.plans().create(plan.clone()).await.unwrap();

    let (events, mut rx) = tokio::sync::broadcast::channel(8);
    let fanout = SummaryFanout::with_events(Arc::clone(&store), target.clone(), Arc::clone(&store), events);

    plan.status = PlanStatus::Stalled;
    PlanUpdateSink::update(&fanout, plan.clone(), vec![], plan.updated_at)
        .await
        .unwrap();
    let event = rx.try_recv().expect("plan.stalled event emitted");
    assert_eq!(event.event, "plan.stalled");
    assert_eq!(event.data["plan_id"], plan.id.to_string());
}
