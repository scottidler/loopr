#![allow(clippy::unwrap_used, unused_imports)]

use serde_json::json;

use crate::agents::AgentKind;
use crate::config::InterviewMode;

use super::fixtures::*;

#[tokio::test]
async fn test_work_fsm_enforcement_via_dispatch() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create hierarchy so work.create can find a valid phase
    let (_plan_id, _spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic).await;

    // Create Work (starts as Draft)
    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "Task", "description": "desc", "files": ["src/"], "acceptance_criteria": ["tests pass"]}),
    ).await;
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // Valid: Ready -> Done(Coordinator) is the pre-flight AC short-circuit path
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "Done", "role": "coordinator"}),
    )
    .await;

    // Verify state is now Done
    let wis = stores.works.read().unwrap();
    assert_eq!(wis[&wi_id].status(), crate::domain::work::WorkStatus::Done);
    drop(wis);

    // Verify at the FSM level that Ready -> Integrated is still invalid
    let fsm = crate::fsm::runtime::FsmInterpreter::embedded().unwrap();
    assert!(
        fsm.validate_transition("work", "ready", "integrated", "coordinator")
            .is_err(),
        "Ready->Integrated should still be an invalid skip state"
    );
    assert!(
        fsm.validate_transition("work", "ready", "in-review", "coordinator")
            .is_err(),
        "Ready->InReview should still be an invalid skip state"
    );
}

// test_full_fsm_cycle removed: CoordinatorState FSM has been deleted.
// Coordinator responsibilities are now handled by the engine.

#[tokio::test]
async fn test_role_inference_from_agent_type() {
    use crate::domain::role::Role;

    assert_eq!(AgentKind::Implementer.default_role(), Role::Implementer);
    assert_eq!(AgentKind::Reviewer.default_role(), Role::Reviewer);
    assert_eq!(AgentKind::Director.default_role(), Role::Director);
    assert_eq!(AgentKind::Researcher.default_role(), Role::Researcher);
    assert_eq!(AgentKind::Integrator.default_role(), Role::Integrator);
}
