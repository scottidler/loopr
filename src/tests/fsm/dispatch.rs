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

fn dispatch_ok(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
    ic: &IntegratorConfig,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let req = DaemonRequest::new(1, method, params);
    let resp = dispatch(stores, tx, wm, ic, req);
    assert!(!resp.is_error(), "{method} failed: {:?}", resp.error);
    resp.result.unwrap()
}

fn dispatch_err(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
    ic: &IntegratorConfig,
    method: &str,
    params: serde_json::Value,
) -> i32 {
    let req = DaemonRequest::new(1, method, params);
    let resp = dispatch(stores, tx, wm, ic, req);
    assert!(
        resp.is_error(),
        "{method} expected error but got success: {:?}",
        resp.result
    );
    resp.error.unwrap().code
}

// --- Plan full lifecycle through dispatch ---

#[test]
fn plan_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    );
    let id = plan["id"].as_str().unwrap().to_string();

    // Draft -> Active
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "active"}),
    );
    let got = dispatch_ok(&s, &tx, &wm, &ic, "plan.get", json!({"id": id}));
    assert_eq!(got["status"], "active");

    // Active -> Complete
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "complete"}),
    );
    let got = dispatch_ok(&s, &tx, &wm, &ic, "plan.get", json!({"id": id}));
    assert_eq!(got["status"], "complete");

    // Complete -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "active"}),
    );
    assert_eq!(code, -32000); // transition_rejected
}

#[test]
fn plan_abandon_from_draft_through_dispatch() {
    let (s, tx, wm, ic) = setup();
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    );
    let id = plan["id"].as_str().unwrap().to_string();

    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "abandoned"}),
    );
    let got = dispatch_ok(&s, &tx, &wm, &ic, "plan.get", json!({"id": id}));
    assert_eq!(got["status"], "abandoned");

    // Abandoned -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "active"}),
    );
    assert_eq!(code, -32000);
}

// --- Work full lifecycle through dispatch ---

#[test]
fn work_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    // Create hierarchy
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    );
    let plan_id = plan["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    );

    let spec = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent_id": plan_id, "title": "S", "description": "D"}),
    );
    let spec_id = spec["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target_status": "active"}),
    );

    let phase = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent_id": spec_id, "title": "Ph", "description": "D"}),
    );
    let phase_id = phase["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target_status": "active"}),
    );

    let wi = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "WI", "description": "D", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
    );
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
    );

    // Create bundle before InReview (precondition)
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "feature/test"}),
    );

    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "InReview", "role": "implementer"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "Integrated", "role": "integrator"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "Done", "role": "coordinator"}),
    );

    let got = dispatch_ok(&s, &tx, &wm, &ic, "work.get", json!({"id": wi_id}));
    assert_eq!(got["status"], "Done");

    // Done -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "Ready", "role": "coordinator"}),
    );
    assert_eq!(code, -32000);
}

// --- Bundle full lifecycle through dispatch ---

#[test]
fn bundle_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    // Create hierarchy + work item
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    );
    let plan_id = plan["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    );
    let spec = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent_id": plan_id, "title": "S", "description": "D"}),
    );
    let spec_id = spec["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target_status": "active"}),
    );
    let phase = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent_id": spec_id, "title": "Ph", "description": "D"}),
    );
    let phase_id = phase["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target_status": "active"}),
    );
    let wi = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "WI", "description": "D", "resource_tags": ["src/"]}),
    );
    let wi_id = wi["id"].as_str().unwrap();

    let bundle = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "feature/test"}),
    );
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
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Accepted", "role": "coordinator"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Integrating", "role": "integrator"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Merged", "role": "integrator"}),
    );

    let got = dispatch_ok(&s, &tx, &wm, &ic, "bundle.get", json!({"id": bid}));
    assert_eq!(got["status"], "Merged");

    // Merged -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bid, "target_status": "Proposed", "role": "coordinator"}),
    );
    assert_eq!(code, -32000);
}

// --- Bundle rejection flow ---

#[test]
fn bundle_rejection_at_every_stage() {
    let (s, tx, wm, ic) = setup();
    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    );
    let plan_id = plan["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    );
    let spec = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent_id": plan_id, "title": "S", "description": "D"}),
    );
    let spec_id = spec["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target_status": "active"}),
    );
    let phase = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent_id": spec_id, "title": "Ph", "description": "D"}),
    );
    let phase_id = phase["id"].as_str().unwrap();
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target_status": "active"}),
    );
    let wi = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "WI", "description": "D", "resource_tags": ["src/"]}),
    );
    let wi_id = wi["id"].as_str().unwrap();

    // Reject from Proposed (Reviewer)
    let b1 = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "f/1"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b1["id"].as_str().unwrap(), "target_status": "Rejected", "role": "reviewer"}),
    );

    // Reject from Triaged (Coordinator)
    let b2 = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "f/2"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b2["id"].as_str().unwrap(), "target_status": "Triaged", "role": "coordinator"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b2["id"].as_str().unwrap(), "target_status": "Rejected", "role": "coordinator"}),
    );

    // Reject from Reviewed (Reviewer)
    let b3 = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "f/3"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b3["id"].as_str().unwrap(), "target_status": "Triaged", "role": "coordinator"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b3["id"].as_str().unwrap(), "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": b3["id"].as_str().unwrap(), "target_status": "Rejected", "role": "reviewer"}),
    );
}

// --- Tick full lifecycle through dispatch ---

#[test]
fn tick_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let tick = dispatch_ok(&s, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
    let tid = tick["id"].as_str().unwrap().to_string();
    assert_eq!(tick["status"], "Open");

    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Sealing", "role": "integrator"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Validating", "role": "integrator"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Published", "role": "integrator"}),
    );

    let got = dispatch_ok(&s, &tx, &wm, &ic, "tick.get", json!({"id": tid}));
    assert_eq!(got["status"], "Published");

    // Published -> anything should fail
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Open", "role": "integrator"}),
    );
    assert_eq!(code, -32000);
}

#[test]
fn tick_failure_path_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let tick = dispatch_ok(&s, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
    let tid = tick["id"].as_str().unwrap().to_string();

    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Sealing", "role": "integrator"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Validating", "role": "integrator"}),
    );
    dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tid, "target_status": "Failed", "role": "integrator"}),
    );

    let got = dispatch_ok(&s, &tx, &wm, &ic, "tick.get", json!({"id": tid}));
    assert_eq!(got["status"], "Failed");
}

// --- Wrong role through dispatch ---

#[test]
fn wrong_role_rejected_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let plan = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "P", "description": "D"}),
    );
    let id = plan["id"].as_str().unwrap();

    // Implementer cannot transition plans
    let code = dispatch_err(
        &s,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": id, "target_status": "active", "role": "implementer"}),
    );
    assert_eq!(code, -32000);
}

// --- Lock lifecycle through dispatch ---

#[test]
fn lock_full_lifecycle_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let lock = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "lock.create",
        json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord"}),
    );
    let lid = lock["id"].as_str().unwrap().to_string();
    assert_eq!(lock["status"], "active");

    dispatch_ok(&s, &tx, &wm, &ic, "lock.release", json!({"id": lid}));
    let got = dispatch_ok(&s, &tx, &wm, &ic, "lock.get", json!({"id": lid}));
    assert_eq!(got["status"], "released");
}

#[test]
fn lock_expire_through_dispatch() {
    let (s, tx, wm, ic) = setup();

    let lock = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "lock.create",
        json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord"}),
    );
    let lid = lock["id"].as_str().unwrap().to_string();

    dispatch_ok(&s, &tx, &wm, &ic, "lock.expire", json!({"id": lid}));
    let got = dispatch_ok(&s, &tx, &wm, &ic, "lock.get", json!({"id": lid}));
    assert_eq!(got["status"], "expired");
}

// --- Tick singleton guard ---

#[test]
fn tick_singleton_guard() {
    let (s, tx, wm, ic) = setup();

    dispatch_ok(&s, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
    // Second non-terminal tick should be rejected
    let code = dispatch_err(&s, &tx, &wm, &ic, "tick.create", json!({"number": 2}));
    assert_eq!(code, -32005); // precondition_failed
}

// --- Learning lifecycle through dispatch ---

#[test]
fn learning_reinforce_contradict_promote_demote() {
    let (s, tx, wm, ic) = setup();

    let learning = dispatch_ok(
        &s,
        &tx,
        &wm,
        &ic,
        "learning.create",
        json!({"content": "Always run tests", "scope": "global", "source_id": "plan-1"}),
    );
    let lid = learning["id"].as_str().unwrap().to_string();
    assert_eq!(learning["reinforcements"], 0);
    assert_eq!(learning["contradictions"], 0);

    // Reinforce 3 times
    for _ in 0..3 {
        dispatch_ok(&s, &tx, &wm, &ic, "learning.reinforce", json!({"id": lid}));
    }
    let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid}));
    assert_eq!(got["reinforcements"], 3);

    // Contradict
    dispatch_ok(&s, &tx, &wm, &ic, "learning.contradict", json!({"id": lid}));
    let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid}));
    assert_eq!(got["contradictions"], 1);

    // Promote
    dispatch_ok(&s, &tx, &wm, &ic, "learning.promote", json!({"id": lid}));
    let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid}));
    assert_eq!(got["promoted"], true);

    // Demote
    dispatch_ok(&s, &tx, &wm, &ic, "learning.demote", json!({"id": lid}));
    let got = dispatch_ok(&s, &tx, &wm, &ic, "learning.get", json!({"id": lid}));
    assert_eq!(got["promoted"], false);
}
