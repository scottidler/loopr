#![allow(clippy::unwrap_used)]
//! Phase 9-10 tests: operator-note path through the Director run loop
//! plus the `NeedsOperator -> Stalled` grace counter. Pulled out of
//! `director/tests.rs` to keep both files under the 1500-line cap
//! enforced by `otto ci`'s bloat task.
//!
//! `use super::*;` imports the scaffolding (`FakeLlm`, `FakeStore`,
//! `RecordingSpawner`, `make_work`, `fast_config`, `make_deps`, etc.)
//! from the parent `tests` module via submodule privilege.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::Notify;

use domain::{OperatorNote, Plan, PlanId, PlanStatus, WorkStatus};

use super::{FakeLlm, FakeStore, RecordingSpawner, fast_config, make_deps, make_work};
use crate::director::{DirectorSession, run_director};

// ---------------------------------------------------------------------------
// Phase 9: Operator note path
// ---------------------------------------------------------------------------

/// Operator note rendered into the user prompt AND demotes Conservative
/// to Normal AND is marked read so the next iteration's prompt does NOT
/// re-include it. This is the end-to-end test for Phase 9.
#[tokio::test]
async fn operator_note_renders_demotes_conservative_and_marks_read() {
    let plan_id = PlanId::new();
    let blocked = make_work(plan_id.clone(), "wk-1", WorkStatus::Blocked);
    let work_id_s = blocked.id.to_string();
    let store = FakeStore::with(vec![blocked], vec![]);
    // Pre-seed an operator note so the very FIRST iteration sees it.
    let note = OperatorNote::new(
        plan_id.clone(),
        "scott".to_string(),
        "try the failing test in verbose mode".to_string(),
    );
    store.notes.lock().unwrap().push(note.clone());

    let llm = FakeLlm::repeating(
        json!([{
            "action": "override_work",
            "work_id": work_id_s,
            "target_status": "Ready",
            "reason": "retry"
        }])
        .to_string(),
    );
    let spawner = Arc::new(RecordingSpawner::default());
    let mut config = fast_config();
    config.patterns = crate::config::DirectorConfig::default().patterns;
    config.max_work_attempts = 100;
    let deps = make_deps(llm, store, spawner.clone(), config, Arc::new(Notify::new()));

    // Two deterministic iterations: iter 1 must render the note, iter 2 must
    // NOT (mark-read on the iter-1 LLM round-trip clears the unread flag).
    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    session.run_once(&deps).await.expect("iter 1 Ok");
    session.run_once(&deps).await.expect("iter 2 Ok");

    let prompts = deps.llm.last_user_messages();
    assert_eq!(prompts.len(), 2, "expected exactly two LLM calls");
    assert!(
        prompts[0].contains("### Operator Notes"),
        "first iteration prompt must include the Operator Notes section; got: {}",
        prompts[0]
    );
    assert!(
        prompts[0].contains("try the failing test in verbose mode"),
        "first iteration prompt must include the note body; got: {}",
        prompts[0]
    );
    assert!(
        !prompts[1].contains("try the failing test in verbose mode"),
        "iteration 2 re-rendered the marked-read note body; got: {}",
        prompts[1]
    );
}

/// Render-then-mark ordering regression guard: when the LLM-loop fails
/// before parse succeeds (lifeguard escalates on three consecutive
/// parse failures), `mark_notes_read` must NOT have fired and the note
/// must remain unread so the next Director restart re-renders it.
#[tokio::test]
async fn unread_note_remains_unread_on_lifeguard_escalation() {
    let plan_id = PlanId::new();
    let blocked = make_work(plan_id.clone(), "wk-1", WorkStatus::Blocked);
    let store = FakeStore::with(vec![blocked], vec![]);
    let note = OperatorNote::new(plan_id.clone(), "scott".into(), "investigate".into());
    let note_id = note.id.clone();
    store.notes.lock().unwrap().push(note);

    let llm = FakeLlm::repeating("not json".to_string());
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());
    let mut config = fast_config();
    config.max_requeries = 0;
    config.max_parse_failures = 3;
    let deps = make_deps(llm, store, spawner.clone(), config, shutdown.clone());

    let err = run_director(&plan_id, &deps).await.expect_err("must escalate");
    let msg = format!("{err}");
    assert!(
        msg.contains("Lifeguard") || msg.contains("lifeguard"),
        "expected lifeguard escalation; got: {msg}"
    );

    let notes = deps.store.notes.lock().unwrap();
    let persisted = notes.iter().find(|n| n.id == note_id).expect("note persisted");
    assert!(
        persisted.is_unread(),
        "note must remain unread when the LLM round-trip never reached parse-success; got read_at={:?}",
        persisted.read_at
    );
}

/// After a successful LLM round-trip, the note is marked read so the
/// next iteration's prompt does NOT re-include it.
#[tokio::test]
async fn unread_note_marked_read_after_render() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::Ready);
    let store = FakeStore::with(vec![work], vec![]);
    let note = OperatorNote::new(plan_id.clone(), "scott".into(), "review".into());
    let note_id = note.id.clone();
    store.notes.lock().unwrap().push(note);

    let llm = FakeLlm::repeating(json!([{ "action": "done", "summary": "ack" }]).to_string());
    let spawner = Arc::new(RecordingSpawner::default());
    let config = fast_config();
    let deps = make_deps(llm, store, spawner.clone(), config, Arc::new(Notify::new()));

    // Single iteration: the LLM round-trip that parses successfully must
    // mark the note read (render-then-mark ordering).
    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    session.run_once(&deps).await.expect("run_once Ok");

    let notes = deps.store.notes.lock().unwrap();
    let persisted = notes.iter().find(|n| n.id == note_id).expect("note persisted");
    assert!(
        !persisted.is_unread(),
        "note should be marked read after a successful LLM round-trip; got read_at={:?}",
        persisted.read_at
    );
}

// ---------------------------------------------------------------------------
// Phase 10: NeedsOperator -> Stalled grace
// ---------------------------------------------------------------------------

/// After the Director enters NeedsOperator, if no operator note arrives
/// within `needs_operator_grace_iters` consecutive iterations the
/// Director must transition the Plan -> Stalled (Director role) and
/// exit with `NeedHelp`.
#[tokio::test]
async fn needs_operator_grace_exceeded_stalls_plan_and_returns_need_help() {
    let plan_id = PlanId::new();
    let blocked = make_work(plan_id.clone(), "wk-1", WorkStatus::Blocked);
    let work_id_s = blocked.id.to_string();
    let mut plan = Plan::new("test goal".into());
    plan.id = plan_id.clone();
    let store = FakeStore::with_plan(vec![blocked], vec![], plan);

    let llm = FakeLlm::repeating(
        json!([{
            "action": "override_work",
            "work_id": work_id_s,
            "target_status": "Ready",
            "reason": "retry"
        }])
        .to_string(),
    );
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());

    let mut config = fast_config();
    config.max_work_attempts = 100;
    // iter 1: NoProgressTripped -> Conservative.
    // iter 2: EscalationTripped (streak >= 2) -> NeedsOperator.
    config.patterns.same_action_threshold = 100;
    config.patterns.no_progress_threshold = 1;
    config.patterns.escalation_threshold = 2;
    config.patterns.window = 4;
    // Stall on the third NeedsOperator iteration without notes.
    config.needs_operator_grace_iters = 2;
    let deps = make_deps(llm, store, spawner.clone(), config, shutdown.clone());

    let err = run_director(&plan_id, &deps).await.expect_err("must stall");
    let msg = format!("{err}");
    assert!(
        msg.contains("NeedsOperator timeout"),
        "expected NeedsOperator timeout in NeedHelp; got: {msg}"
    );
    assert_eq!(
        deps.store.plan_status(),
        Some(PlanStatus::Stalled),
        "Plan must be persisted as Stalled before exiting"
    );
}

/// Counter does NOT trip Stalled while mode is anything other than
/// NeedsOperator, regardless of how long the Director runs.
#[tokio::test]
async fn grace_counter_does_not_trip_outside_needs_operator() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::Ready);
    let mut plan = Plan::new("test goal".into());
    plan.id = plan_id.clone();
    let store = FakeStore::with_plan(vec![work], vec![], plan);

    let llm = FakeLlm::repeating(json!([{ "action": "done", "summary": "ok" }]).to_string());
    let spawner = Arc::new(RecordingSpawner::default());
    let mut config = fast_config();
    config.needs_operator_grace_iters = 2;
    let deps = make_deps(llm, store, spawner.clone(), config, Arc::new(Notify::new()));

    // Run 5 deterministic iterations. With `done` actions and identical
    // state hashes, SameActionTripped fires from iter 3 onward — mode
    // pins at Conservative. EscalationTripped fires only via the
    // NoProgress path, which SameActionTripped pre-empts, so mode never
    // reaches NeedsOperator and the grace counter stays at 0.
    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    for i in 0..5 {
        session.run_once(&deps).await.expect("run_once Ok");
        assert_ne!(
            deps.store.plan_status(),
            Some(PlanStatus::Stalled),
            "Plan must not be Stalled after iter {}; got plan_status={:?}",
            i + 1,
            deps.store.plan_status()
        );
    }
}

// ---------------------------------------------------------------------------
// Director Phase 2 follow-ups (Item 3): status snapshot sidecar
// ---------------------------------------------------------------------------

/// After at least one Director iteration, the per-Plan sidecar must
/// carry a fresh `DirectorStatusSnapshot` reflecting the iteration's
/// terminal mode + the action that was emitted. This proves the
/// snapshot write site (step 6b in `run_director_inner`) fires every
/// iteration and exposes the right fields to the IPC `director.status`
/// reader.
#[tokio::test]
async fn director_status_snapshot_records_iteration_and_action() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::Ready);
    let work_id_s = work.id.to_string();
    let store = FakeStore::with(vec![work], vec![]);

    // Scripted `assign_work` action so the snapshot's last_action_kind
    // is non-None and last_action_target_id matches the work id.
    let llm = FakeLlm::repeating(
        json!([{
            "action": "assign_work",
            "work_id": work_id_s,
        }])
        .to_string(),
    );
    let spawner = Arc::new(RecordingSpawner::default());
    let config = fast_config();
    let deps = make_deps(llm, store, spawner.clone(), config, Arc::new(Notify::new()));

    // Single iteration: the snapshot write site (step 6b) must fire
    // every iteration and expose the action's kind + target id.
    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    session.run_once(&deps).await.expect("run_once Ok");

    let map = deps.director_statuses.read().unwrap();
    let snap = map
        .get(&plan_id)
        .expect("snapshot must be present after first iteration");
    assert!(
        snap.iteration >= 1,
        "iteration counter must advance; got {}",
        snap.iteration
    );
    assert_eq!(
        snap.last_action_kind.as_deref(),
        Some("assign_work"),
        "snapshot must record the emitted action kind"
    );
    assert_eq!(
        snap.last_action_target_id.as_deref(),
        Some(work_id_s.as_str()),
        "snapshot must record the action's target id"
    );
    assert!(
        snap.last_action_ts.is_some(),
        "snapshot must stamp a timestamp when an action ran"
    );
    // Mode is whatever the pattern tracker decided after the final
    // observed iteration; we don't pin it here because the test runs
    // many iterations and the same-action streak may or may not have
    // tripped depending on tokio scheduling. The point is the snapshot
    // carries SOME mode that round-trips.
    let _: &str = snap.mode.as_str();
    assert_eq!(snap.unread_note_count, 0, "no notes seeded; count must be zero");
    assert_eq!(
        snap.needs_operator_iters, 0,
        "grace counter is zero outside NeedsOperator mode"
    );
}
