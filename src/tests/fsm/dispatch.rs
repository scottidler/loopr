use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;

use crate::config::IntegratorConfig;
use crate::daemon::context::Stores;
use crate::daemon::handlers::dispatch;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
use crate::worktree::manager::WorktreeManager;

use std::path::PathBuf;

fn setup() -> (
    Arc<Stores>,
    broadcast::Sender<DaemonEvent>,
    WorktreeManager,
    IntegratorConfig,
) {
    let stores = Arc::new(Stores::new());
    let (tx, _) = broadcast::channel(64);
    let wm = WorktreeManager::new(PathBuf::from("/tmp/noop"), PathBuf::from("/tmp/noop-wt"));
    let ic = IntegratorConfig {
        validation_commands: vec!["echo ok".to_string()],
        ..Default::default()
    };
    (stores, tx, wm, ic)
}

async fn dispatch_ok(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
    ic: &IntegratorConfig,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let req = DaemonRequest::new(1, method, params);
    let resp = dispatch(stores, tx, wm, ic, req).await;
    assert!(!resp.is_error(), "{method} failed: {:?}", resp.error);
    resp.result.unwrap()
}

async fn dispatch_err(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
    ic: &IntegratorConfig,
    method: &str,
    params: serde_json::Value,
) -> i32 {
    let req = DaemonRequest::new(1, method, params);
    let resp = dispatch(stores, tx, wm, ic, req).await;
    assert!(
        resp.is_error(),
        "{method} expected error but got success: {:?}",
        resp.result
    );
    resp.error.unwrap().code
}

// --- Plan full lifecycle through dispatch ---

#[tokio::test]
async fn plan_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    )
    .await;
    let id = plan["id"].as_str().unwrap().to_string();

    // Draft -> Active
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "active"}),
    )
    .await;
    let got = dispatch_ok(&s, &tx, &wm, &ic, "plan.get", json!({"id": id})).await;
    assert_eq!(got["status"], "active");

    // Active -> Complete
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "complete"}),
    )
    .await;
    let got = dispatch_ok(&s, &tx, &wm, &ic, "plan.get", json!({"id": id})).await;
    assert_eq!(got["status"], "complete");

    // Complete -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "active"}),
    )
    .await;
    assert_eq!(code, -32000); // transition_rejected
}

#[tokio::test]
async fn plan_abandon_from_draft_through_dispatch() {
    let (s, tx, wm, ic) = setup();
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    )
    .await;
    let id = plan["id"].as_str().unwrap().to_string();

    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "abandoned"}),
    )
    .await;
    let got = dispatch_ok(&s, &tx, &wm, &ic, "plan.get", json!({"id": id})).await;
    assert_eq!(got["status"], "abandoned");

    // Abandoned -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "active"}),
    )
    .await;
    assert_eq!(code, -32000);
}

// --- Work full lifecycle through dispatch ---

#[tokio::test]
async fn work_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    // Create hierarchy
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    )
    .await;

    let spec = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent_id": plan_id, "title": "S", "description": "D"}),
    )
    .await;
    let spec_id = spec["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target_status": "active"}),
    )
    .await;

    let phase = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent_id": spec_id, "title": "Ph", "description": "D"}),
    )
    .await;
    let phase_id = phase["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target_status": "active"}),
    )
    .await;

    let wi = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "WI", "description": "D", "files": ["src/"], "acceptance_criteria": ["tests pass"]}),
    )
    .await;
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // Ready -> InProgress -> InReview -> Integrated -> Done
    // (auto-promoted from Draft to Ready since acceptance_criteria present)
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
    )
    .await;

    // Create bundle before InReview (precondition)
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "feature/test"}),
    )
    .await;

    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "InReview", "role": "implementer"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "Integrated", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "Done", "role": "coordinator"}),
    )
    .await;

    let got = dispatch_ok(&s, &tx, &wm, &ic, "work.get", json!({"id": wi_id})).await;
    assert_eq!(got["status"], "Done");

    // Done -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "Ready", "role": "coordinator"}),
    )
    .await;
    assert_eq!(code, -32000);
}

// --- Bundle full lifecycle through dispatch ---

#[tokio::test]
async fn bundle_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    // Create hierarchy + work item
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    )
    .await;
    let spec = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent_id": plan_id, "title": "S", "description": "D"}),
    )
    .await;
    let spec_id = spec["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target_status": "active"}),
    )
    .await;
    let phase = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent_id": spec_id, "title": "Ph", "description": "D"}),
    )
    .await;
    let phase_id = phase["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target_status": "active"}),
    )
    .await;
    let wi = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "WI", "description": "D", "files": ["src/"]}),
    )
    .await;
    let wi_id = wi["id"].as_str().unwrap();

    let bundle = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "feature/test"}),
    )
    .await;
    let bid = bundle["id"].as_str().unwrap().to_string();
    assert_eq!(bundle["status"], "Proposed");

    // Proposed -> Triaged -> Reviewed -> Accepted -> Integrating -> Merged
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Triaged", "role": "coordinator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Accepted", "role": "coordinator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Integrating", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Merged", "role": "integrator"}),
    )
    .await;

    let got = dispatch_ok(&s, &tx, &wm, &ic, "bundle.get", json!({"id": bid})).await;
    assert_eq!(got["status"], "Merged");

    // Merged -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Proposed", "role": "coordinator"}),
    )
    .await;
    assert_eq!(code, -32000);
}

// --- Bundle rejection flow ---

#[tokio::test]
async fn bundle_rejection_at_every_stage() {
    let (s, tx, wm, ic) = setup();
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    )
    .await;
    let spec = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent_id": plan_id, "title": "S", "description": "D"}),
    )
    .await;
    let spec_id = spec["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target_status": "active"}),
    )
    .await;
    let phase = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent_id": spec_id, "title": "Ph", "description": "D"}),
    )
    .await;
    let phase_id = phase["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target_status": "active"}),
    )
    .await;
    let wi = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "WI", "description": "D", "files": ["src/"]}),
    )
    .await;
    let wi_id = wi["id"].as_str().unwrap();

    // Reject from Proposed (Reviewer)
    let b1 = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "f/1"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b1["id"].as_str().unwrap(), "target_status": "Rejected", "role": "reviewer"}),
    )
    .await;

    // Reject from Triaged (Coordinator)
    let b2 = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "f/2"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b2["id"].as_str().unwrap(), "target_status": "Triaged", "role": "coordinator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b2["id"].as_str().unwrap(), "target_status": "Rejected", "role": "coordinator"}),
    )
    .await;

    // Reject from Reviewed (Reviewer)
    let b3 = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "f/3"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b3["id"].as_str().unwrap(), "target_status": "Triaged", "role": "coordinator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b3["id"].as_str().unwrap(), "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b3["id"].as_str().unwrap(), "target_status": "Rejected", "role": "reviewer"}),
    )
    .await;
}

// --- Tick full lifecycle through dispatch ---

#[tokio::test]
async fn tick_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let tick = dispatch_ok(&s, &tx, &wm, &ic, "tick.create", json!({"number": 1})).await;
    let tid = tick["id"].as_str().unwrap().to_string();
    assert_eq!(tick["status"], "Open");

    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Sealing", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Validating", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Published", "role": "integrator"}),
    )
    .await;

    let got = dispatch_ok(&s, &tx, &wm, &ic, "tick.get", json!({"id": tid})).await;
    assert_eq!(got["status"], "Published");

    // Published -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Open", "role": "integrator"}),
    )
    .await;
    assert_eq!(code, -32000);
}

#[tokio::test]
async fn tick_failure_path_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let tick = dispatch_ok(&s, &tx, &wm, &ic, "tick.create", json!({"number": 1})).await;
    let tid = tick["id"].as_str().unwrap().to_string();

    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Sealing", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Validating", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Failed", "role": "integrator"}),
    )
    .await;

    let got = dispatch_ok(&s, &tx, &wm, &ic, "tick.get", json!({"id": tid})).await;
    assert_eq!(got["status"], "Failed");
}

// --- Wrong role through dispatch ---

#[tokio::test]
async fn wrong_role_rejected_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    )
    .await;
    let id = plan["id"].as_str().unwrap();

    // Implementer cannot transition plans
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "active", "role": "implementer"}),
    )
    .await;
    assert_eq!(code, -32000);
}

// --- Lock lifecycle through dispatch ---

#[tokio::test]
async fn lock_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let lock = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "lock.create",
        json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord"}),
    )
    .await;
    let lid = lock["id"].as_str().unwrap().to_string();
    assert_eq!(lock["status"], "active");

    dispatch_ok(&s, &tx, &wm, &ic, "lock.release", json!({"id": lid})).await;
    let got = dispatch_ok(&s, &tx, &wm, &ic, "lock.get", json!({"id": lid})).await;
    assert_eq!(got["status"], "released");
}

#[tokio::test]
async fn lock_expire_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let lock = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "lock.create",
        json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord"}),
    )
    .await;
    let lid = lock["id"].as_str().unwrap().to_string();

    dispatch_ok(&s, &tx, &wm, &ic, "lock.expire", json!({"id": lid})).await;
    let got = dispatch_ok(&s, &tx, &wm, &ic, "lock.get", json!({"id": lid})).await;
    assert_eq!(got["status"], "expired");
}

// --- Tick singleton guard ---

#[tokio::test]
async fn tick_singleton_guard() {
    let (s, tx, wm, ic) = setup();

    dispatch_ok(&s, &tx, &wm, &ic, "tick.create", json!({"number": 1})).await;
    // Second non-terminal tick should be rejected
    let code = dispatch_err(&s, &tx, &wm, &ic, "tick.create", json!({"number": 2})).await;
    assert_eq!(code, -32005); // precondition_failed
}

// --- Learning lifecycle through dispatch ---

#[tokio::test]
async fn learning_reinforce_contradict_promote_demote() {
    let (s, tx, wm, ic) = setup();

    let learning = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "learning.create",
        json!({"content": "Always run tests", "scope": "global", "source_id": "plan-1"}),
    )
    .await;
    let lid = learning["id"].as_str().unwrap().to_string();
    assert_eq!(learning["reinforcements"], 0);
    assert_eq!(learning["contradictions"], 0);

    // Reinforce 3 times
    for _ in 0..3 {
        dispatch_ok(&s, &tx, &wm, &ic, "learning.reinforce", json!({"id": lid})).await;
    }
    let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid})).await;
    assert_eq!(got["reinforcements"], 3);

    // Contradict
    dispatch_ok(&s, &tx, &wm, &ic, "learning.contradict", json!({"id": lid})).await;
    let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid})).await;
    assert_eq!(got["contradictions"], 1);

    // Promote
    dispatch_ok(&s, &tx, &wm, &ic, "learning.promote", json!({"id": lid})).await;
    let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid})).await;
    assert_eq!(got["promoted"], true);

    // Demote
    dispatch_ok(&s, &tx, &wm, &ic, "learning.demote", json!({"id": lid})).await;
    let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid})).await;
    assert_eq!(got["promoted"], false);
}
