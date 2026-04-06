#![allow(clippy::unwrap_used, unused_imports)]

use serde_json::json;

use crate::agents::AgentAction;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::executor::{ActionResult, execute_action};
use crate::test_util::TestDir;

use super::fixtures::*;

#[tokio::test(flavor = "multi_thread")]
async fn test_coordinator_creates_work_via_executor() {
    // Use create_test_hierarchy fixture to build Plan/Spec/Phase, then verify CreateWork via executor
    let dir = TestDir::new("loopr-e2e-hierarchy");

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();
    let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm.clone(), stores.config.clone());
    let ctx = test_agent_context(&stores, bridge, tx.clone());

    let (plan_id, spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic).await;

    // Create Work via executor (the live path)
    let wi_result = execute_action(
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
    )
    .await
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

#[tokio::test(flavor = "multi_thread")]
async fn test_coordinator_accept_bundle_via_executor() {
    // Create hierarchy + bundle -> (triage via daemon) -> AcceptBundle through executor
    use crate::domain::bundle::BundleStatus;

    let dir = TestDir::new("loopr-e2e-triage");

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();
    let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm.clone(), stores.config.clone());

    // Create hierarchy + work item (via dispatch for speed)
    let (_, _, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic).await;
    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "WI", "description": "d", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
    ).await;
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // Transition WI to InProgress so bundle can be proposed (already Ready via auto-promotion)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
    )
    .await;

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
    )
    .await;
    let bundle_id = bundle["id"].as_str().unwrap().to_string();

    // Triage is now automatic via daemon auto_start_agents (Fix 7).
    // In tests the daemon hook doesn't fire, so triage directly via bridge.
    let ctx = test_agent_context(&stores, bridge, tx.clone());
    ctx.bridge.request(
        "bundle.transition",
        serde_json::json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
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
    )
    .await;

    // AcceptBundle via executor
    let accept_result = execute_action(
        &AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        },
        &ctx,
        &dir,
        None,
    )
    .await
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
