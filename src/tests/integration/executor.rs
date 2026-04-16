#![allow(clippy::unwrap_used, unused_imports)]

use serde_json::json;

use crate::agents::AgentAction;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::executor::{ActionResult, execute_action};
use crate::test_util::TestDir;

use super::fixtures::*;

#[tokio::test(flavor = "multi_thread")]
async fn test_director_creates_work_via_executor() {
    // Use create_test_hierarchy fixture to build Plan/Spec/Phase, then verify CreateWork via executor
    let dir = TestDir::new("loopr-e2e-hierarchy");

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        tx.clone(),
        wm.clone(),
        stores.config.clone(),
        stores.fsm.clone(),
    );
    let ctx = test_agent_context(&stores, bridge, tx.clone());

    let (plan_id, spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic).await;

    // Create Work via executor (the live path)
    let wi_result = execute_action(
        &AgentAction::CreateWork {
            parent_id: phase_id.clone(),
            title: "Add login".into(),
            description: "Add login endpoint".into(),
            files: vec!["src/".into()],
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
