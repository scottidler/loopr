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
        json!({"parent_id": phase_id, "title": "Task", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
    ).await;
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // Invalid: Ready -> Done (must go through InProgress first)
    let code = dispatch_err(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "Done", "role": "coordinator"}),
    )
    .await;
    assert_ne!(code, 0, "should reject invalid transition");

    // Verify state unchanged (auto-promoted to Ready since acceptance_criteria present)
    let wis = stores.works.read().unwrap();
    assert_eq!(wis[&wi_id].status(), crate::domain::work::WorkStatus::Ready);
}

#[tokio::test]
async fn test_full_fsm_cycle() {
    use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};

    let stores = test_stores();
    let goal_id = "goal-test-fsm".to_string();

    // Insert CoordinatorState directly (the Coordinator agent would normally do this)
    let mut state = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
    let state_id = state.id.clone();
    assert_eq!(state.fsm_state, CoordinatorFsmState::Interviewing);

    // Transition through all states: Interviewing -> Planning -> ActivatePhase -> ...
    state.transition_to(CoordinatorFsmState::Planning);
    assert_eq!(state.fsm_state, CoordinatorFsmState::Planning);

    state.transition_to(CoordinatorFsmState::ActivatePhase);
    assert_eq!(state.fsm_state, CoordinatorFsmState::ActivatePhase);

    state.activate_phase("phase-1".to_string());
    assert_eq!(state.fsm_state, CoordinatorFsmState::Executing);
    assert_eq!(state.current_phase_id.as_deref(), Some("phase-1"));

    state.transition_to(CoordinatorFsmState::PhaseGate);
    assert_eq!(state.fsm_state, CoordinatorFsmState::PhaseGate);

    state.complete_phase();
    assert_eq!(state.phases_completed, vec!["phase-1"]);
    assert!(state.current_phase_id.is_none());

    state.transition_to(CoordinatorFsmState::GoalComplete);
    assert!(state.fsm_state.is_terminal());

    // Verify state persists in stores
    stores
        .coordinator_states
        .write()
        .unwrap()
        .insert(state_id.clone(), state.clone());

    let retrieved = stores
        .coordinator_states
        .read()
        .unwrap()
        .get(&state_id)
        .cloned()
        .unwrap();
    assert_eq!(retrieved.fsm_state, CoordinatorFsmState::GoalComplete);
    assert_eq!(retrieved.goal_id, goal_id);
    assert_eq!(retrieved.phases_completed, vec!["phase-1"]);
}

#[tokio::test]
async fn test_role_inference_from_agent_type() {
    use crate::domain::role::Role;

    assert_eq!(AgentKind::Implementer.default_role(), Role::Implementer);
    assert_eq!(AgentKind::Reviewer.default_role(), Role::Reviewer);
    assert_eq!(AgentKind::Coordinator.default_role(), Role::Coordinator);
    assert_eq!(AgentKind::Researcher.default_role(), Role::Researcher);
    assert_eq!(AgentKind::Integrator.default_role(), Role::Integrator);
}
