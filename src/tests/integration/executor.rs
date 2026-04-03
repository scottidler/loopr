#![allow(clippy::unwrap_used)]

use serde_json::json;

use crate::agents::AgentAction;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::executor::{ActionResult, execute_action};
use crate::test_util::TestDir;
use crate::worktree::manager::WorktreeManager;

use super::fixtures::*;

#[test]
fn test_coordinator_action_creates_plan_via_executor() {
    // Verify that CreatePlan action through execute_action actually creates a plan in stores
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = TestDir::new("loopr-e2e-createplan");

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm, stores.config.clone());

    let action = AgentAction::CreatePlan {
        title: "E2E Test Plan".to_string(),
        description: "Test description".to_string(),
        acceptance_criteria: "All tests pass".to_string(),
    };

    let agent_log = test_agent_logger(&dir);
    let ctx = test_agent_context(&stores, bridge, tx, agent_log);
    let result = rt.block_on(execute_action(&action, &ctx, &dir, None)).unwrap();

    match result {
        ActionResult::RecordCreated { collection, id } => {
            assert_eq!(collection, "plans");
            // Verify plan exists in stores
            let plans = stores.plans.read().unwrap();
            let plan = plans.get(&id).expect("plan should exist in stores");
            assert_eq!(plan.title, "E2E Test Plan");
        }
        other => panic!("expected RecordCreated, got: {:?}", other),
    }
}

#[test]
fn test_coordinator_creates_full_hierarchy_via_executor() {
    // CreatePlan -> CreateSpec -> CreatePhase -> CreateWork through executor
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = TestDir::new("loopr-e2e-hierarchy");

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm, stores.config.clone());
    let agent_log = test_agent_logger(&dir);
    let ctx = test_agent_context(&stores, bridge, tx, agent_log);

    // Create Plan
    let plan_result = rt
        .block_on(execute_action(
            &AgentAction::CreatePlan {
                title: "Auth Plan".into(),
                description: "Auth system".into(),
                acceptance_criteria: "Tests pass".into(),
            },
            &ctx,
            &dir,
            None,
        ))
        .unwrap();
    let plan_id = match plan_result {
        ActionResult::RecordCreated { id, .. } => id,
        other => panic!("expected RecordCreated for plan, got: {:?}", other),
    };

    // Create Spec
    let spec_result = rt
        .block_on(execute_action(
            &AgentAction::CreateSpec {
                plan_id: plan_id.clone(),
                title: "JWT Spec".into(),
                description: "JWT tokens".into(),
            },
            &ctx,
            &dir,
            None,
        ))
        .unwrap();
    let spec_id = match spec_result {
        ActionResult::RecordCreated { id, .. } => id,
        other => panic!("expected RecordCreated for spec, got: {:?}", other),
    };

    // Create Phase
    let phase_result = rt
        .block_on(execute_action(
            &AgentAction::CreatePhase {
                spec_id: spec_id.clone(),
                title: "Phase 1".into(),
                description: "Foundation".into(),
                order: 1,
            },
            &ctx,
            &dir,
            None,
        ))
        .unwrap();
    let phase_id = match phase_result {
        ActionResult::RecordCreated { id, .. } => id,
        other => panic!("expected RecordCreated for phase, got: {:?}", other),
    };

    // Create Work
    let wi_result = rt
        .block_on(execute_action(
            &AgentAction::CreateWork {
                phase_id: phase_id.clone(),
                title: "Add login".into(),
                description: "Add login endpoint".into(),
                resource_tags: vec!["src/".into()],
                acceptance_criteria: vec!["tests pass".into()],
                dependencies: vec![],
            },
            &ctx,
            &dir,
            None,
        ))
        .unwrap();
    let wi_id = match wi_result {
        ActionResult::RecordCreated { collection, id } => {
            assert_eq!(collection, "works");
            id
        }
        other => panic!("expected RecordCreated for work, got: {:?}", other),
    };

    // Verify all records exist with correct parent linkage
    let plans = stores.plans.read().unwrap();
    assert!(plans.contains_key(&plan_id));

    let specs = stores.specs.read().unwrap();
    let spec = specs.get(&spec_id).unwrap();
    assert_eq!(spec.plan_id, plan_id);

    let phases = stores.phases.read().unwrap();
    let phase = phases.get(&phase_id).unwrap();
    assert_eq!(phase.spec_id, spec_id);

    let wis = stores.works.read().unwrap();
    let wi = wis.get(&wi_id).unwrap();
    assert_eq!(wi.phase_id, phase_id);
}

#[test]
fn test_coordinator_triage_accept_bundle_via_executor() {
    // Create hierarchy + bundle -> TriageBundle -> AcceptBundle through executor
    use crate::domain::bundle::BundleStatus;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = TestDir::new("loopr-e2e-triage");

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();
    let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm.clone(), stores.config.clone());

    // Create hierarchy + work item (via dispatch for speed)
    let (_, _, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);
    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"phase_id": phase_id, "title": "WI", "description": "d", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
    );
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // Transition WI to InProgress so bundle can be proposed (already Ready via auto-promotion)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
    );

    // Create a bundle
    let bundle = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "description": "Auth changes",
            "files_changed": ["src/auth.rs"],
            "commit_sha": "abc123",
            "branch_name": "feature-auth"
        }),
    );
    let bundle_id = bundle["id"].as_str().unwrap().to_string();

    // TriageBundle via executor
    let agent_log = test_agent_logger(&dir);
    let ctx = test_agent_context(&stores, bridge, tx.clone(), agent_log);
    let triage_result = rt
        .block_on(execute_action(
            &AgentAction::TriageBundle {
                bundle_id: bundle_id.clone(),
            },
            &ctx,
            &dir,
            None,
        ))
        .unwrap();
    assert!(
        matches!(triage_result, ActionResult::Transitioned(ref s) if s.contains("Triaged")),
        "expected Transitioned(Triaged), got: {:?}",
        triage_result
    );

    // Verify bundle is Triaged
    {
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status(), BundleStatus::Triaged);
    }

    // Review the bundle (needed before Accept)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    );

    // AcceptBundle via executor
    let accept_result = rt
        .block_on(execute_action(
            &AgentAction::AcceptBundle {
                bundle_id: bundle_id.clone(),
            },
            &ctx,
            &dir,
            None,
        ))
        .unwrap();
    assert!(
        matches!(accept_result, ActionResult::Transitioned(ref s) if s.contains("Accepted")),
        "expected Transitioned(Accepted), got: {:?}",
        accept_result
    );

    // Verify bundle is Accepted
    {
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status(), BundleStatus::Accepted);
    }
}
