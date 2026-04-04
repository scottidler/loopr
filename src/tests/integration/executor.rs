#![allow(clippy::unwrap_used)]

use serde_json::json;

use crate::agents::AgentAction;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::executor::{ActionResult, execute_action};
use crate::test_util::TestDir;

use super::fixtures::*;

#[test]
fn test_coordinator_creates_work_via_executor() {
    // Use create_test_hierarchy fixture to build Plan/Spec/Phase, then verify CreateWork via executor
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = TestDir::new("loopr-e2e-hierarchy");

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();
    let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm.clone(), stores.config.clone());
    let agent_log = test_agent_logger(&dir);
    let ctx = test_agent_context(&stores, bridge, tx.clone(), agent_log);

    let (plan_id, spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

    // Create Work via executor (the live path)
    let wi_result = rt
        .block_on(execute_action(
            &AgentAction::CreateWork {
                parent_id: phase_id.clone(),
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
    assert_eq!(spec.parent_id, plan_id);

    let phases = stores.phases.read().unwrap();
    let phase = phases.get(&phase_id).unwrap();
    assert_eq!(phase.parent_id, spec_id);

    let wis = stores.works.read().unwrap();
    let wi = wis.get(&wi_id).unwrap();
    assert_eq!(wi.parent_id, phase_id);
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
        json!({"parent_id": phase_id, "title": "WI", "description": "d", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
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
