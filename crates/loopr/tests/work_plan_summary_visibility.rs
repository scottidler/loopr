//! Phase 8 contract test from
//! `docs/design/2026-05-09-comprehensive-telemetry.md`. Drives a Work
//! and a Plan through their terminal-state transitions under the
//! production telemetry subscriber and asserts events.log carries the
//! `work: terminal-state summary` and `plan: terminal-state summary`
//! events with their declared fields.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::Mutex;

// Both scenarios install a thread-local `set_default` telemetry
// subscriber via `init_for_test` and read back their own tempdir
// `events.log`. Running them concurrently in one test binary races on
// subscriber capture (events intermittently land in the wrong run-dir),
// so serialize them. An async (tokio) Mutex is held across the tests'
// `.await` points without the std-Mutex `await_holding_lock` hazard.
static TELEMETRY_SERIAL: Mutex<()> = Mutex::const_new(());

use domain::{AcceptanceCriteria, Plan, PlanStatus, Role, Work, WorkStatus};
use loopr::daemon::context::{PlanSummaryExtras, transition_and_persist_plan, transition_and_persist_work};
use store::Store;

// ---------- Harness ----------

fn read_events(run_dir: &Path) -> Vec<Value> {
    let body = fs::read_to_string(run_dir.join("events.log")).expect("read events.log");
    body.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("parse JSONL line {line:?}: {e}")))
        .collect()
}

fn find_event<'a>(events: &'a [Value], message: &str) -> Option<&'a Value> {
    events
        .iter()
        .find(|ev| ev.get("fields").and_then(|f| f.get("message")).and_then(|v| v.as_str()) == Some(message))
}

// ---------- Scenario ----------

#[tokio::test(flavor = "current_thread")]
async fn work_terminal_summary_emits_on_done_transition() {
    let _serial = TELEMETRY_SERIAL.lock().await;
    let log_dir = TempDir::new().unwrap();
    let target_dir = TempDir::new().unwrap();

    let store = Store::open(target_dir.path()).await.unwrap();
    let plan = Plan::new("phase 8 plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();

    let mut work = Work::new(plan.id.clone(), "do thing".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["thing done".to_string()]);
    store.works().create(work.clone()).await.unwrap();

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        // Walk the FSM with the role each edge requires per domain::Work
        // FSM: Reactor for Pending/Ready/Integrated/Done transitions,
        // Implementer for InProgress -> InReview.
        let walk: &[(WorkStatus, Role)] = &[
            (WorkStatus::Ready, Role::Reactor),
            (WorkStatus::InProgress, Role::Reactor),
            (WorkStatus::InReview, Role::Implementer),
            (WorkStatus::Integrated, Role::Integrator),
            (WorkStatus::Done, Role::Reactor),
        ];
        for &(next, role) in walk {
            transition_and_persist_work::<Store>(&store, &mut work, next, role, false)
                .await
                .expect("transition ok");
        }
    }
    store.close().await.unwrap();

    let events = read_events(log_dir.path());
    let summary = find_event(&events, "work: terminal-state summary")
        .expect("expected `work: terminal-state summary` event on Done transition");
    let f = summary.get("fields").expect("fields");
    assert!(f.get("work_id").is_some(), "missing work_id");
    assert!(f.get("plan_id").is_some(), "missing plan_id");
    assert!(f.get("terminal_state").is_some(), "missing terminal_state");
    assert!(f.get("attempt_count").is_some(), "missing attempt_count");

    // Exactly one terminal summary event for one terminal transition.
    let count = events
        .iter()
        .filter(|ev| {
            ev.get("fields").and_then(|f| f.get("message")).and_then(|v| v.as_str())
                == Some("work: terminal-state summary")
        })
        .count();
    assert_eq!(count, 1, "expected exactly one terminal-state summary, got {count}");
}

#[tokio::test(flavor = "current_thread")]
async fn plan_terminal_summary_emits_on_complete_transition() {
    let _serial = TELEMETRY_SERIAL.lock().await;
    let log_dir = TempDir::new().unwrap();
    let target_dir = TempDir::new().unwrap();

    let store = Store::open(target_dir.path()).await.unwrap();
    let mut plan = Plan::new("phase 8 plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();

    let mut work_a = Work::new(plan.id.clone(), "a".to_string());
    work_a.acceptance_criteria = AcceptanceCriteria(vec!["a done".to_string()]);
    store.works().create(work_a.clone()).await.unwrap();
    let mut work_b = Work::new(plan.id.clone(), "b".to_string());
    work_b.acceptance_criteria = AcceptanceCriteria(vec!["b done".to_string()]);
    store.works().create(work_b.clone()).await.unwrap();

    // Drive both Works to Done before transitioning the Plan, so the
    // Plan summary's children-vector mirrors a real complete run.
    let walk: &[(WorkStatus, Role)] = &[
        (WorkStatus::Ready, Role::Reactor),
        (WorkStatus::InProgress, Role::Reactor),
        (WorkStatus::InReview, Role::Implementer),
        (WorkStatus::Integrated, Role::Integrator),
        (WorkStatus::Done, Role::Reactor),
    ];
    for &(next, role) in walk {
        transition_and_persist_work::<Store>(&store, &mut work_a, next, role, false)
            .await
            .expect("a transition ok");
        transition_and_persist_work::<Store>(&store, &mut work_b, next, role, false)
            .await
            .expect("b transition ok");
    }
    let children = vec![work_a.clone(), work_b.clone()];

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        // Plan::new starts at Active per docs/design (Stage 5 has no clarity
        // loop); transition Active -> Complete to land on the terminal
        // summary.
        transition_and_persist_plan::<Store>(
            &store,
            &mut plan,
            children.clone(),
            PlanStatus::Complete,
            Role::Reactor,
            PlanSummaryExtras::default(),
            false,
        )
        .await
        .expect("plan transition ok");
    }
    store.close().await.unwrap();

    let events = read_events(log_dir.path());
    let summary = find_event(&events, "plan: terminal-state summary")
        .expect("expected `plan: terminal-state summary` event on Complete transition");
    let f = summary.get("fields").expect("fields");
    assert!(f.get("plan_id").is_some(), "missing plan_id");
    assert!(f.get("terminal_state").is_some(), "missing terminal_state");
    assert_eq!(
        f.get("total_works").and_then(|v| v.as_u64()),
        Some(2),
        "expected 2 child Works"
    );
    assert_eq!(
        f.get("works_done").and_then(|v| v.as_u64()),
        Some(2),
        "both children should be Done"
    );
    // Phase 8 extras: present (== 0 here because the test uses
    // PlanSummaryExtras::default(); production callers populate
    // them from store queries).
    assert_eq!(f.get("ticks").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(f.get("bundles_accepted").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(f.get("bundles_rejected").and_then(|v| v.as_u64()), Some(0));
}
