#![allow(clippy::unwrap_used, unused_imports)]

use serde_json::json;

use super::fixtures::*;

/*
#[test]
fn test_full_hierarchy_creation_via_dispatch() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create Plan
    let plan = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "Auth System", "description": "Add authentication", "acceptance_criteria": "All tests pass"}),
    );
    let plan_id = plan["id"].as_str().unwrap().to_string();
    assert_eq!(plan["status"], "draft");

    // Transition Plan: Draft -> Active (no validator, so no gate)
    let plan_active = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    );
    assert_eq!(plan_active["status"], "active");

    // Create Spec under Plan
    let spec = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent_id": plan_id, "title": "JWT Auth", "description": "Implement JWT-based auth", "acceptance_criteria": "JWT tokens work"}),
    );
    let spec_id = spec["id"].as_str().unwrap().to_string();
    assert_eq!(spec["status"], "draft");

    // Transition Spec: Draft -> Active
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target_status": "active"}),
    );

    // Create Phase under Spec
    let phase = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent_id": spec_id, "title": "Token Generation", "description": "Create token gen module", "acceptance_criteria": "Tokens are signed"}),
    );
    let phase_id = phase["id"].as_str().unwrap().to_string();

    // Transition Phase: Draft -> Active
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target_status": "active"}),
    );

    // Create Work under Phase
    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "Implement sign()", "description": "JWT signing function", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
    );
    let wi_id = wi["id"].as_str().unwrap().to_string();
    assert_eq!(wi["status"], "Ready");

    // Transition Work: Ready -> InProgress (auto-promoted from Draft since acceptance_criteria present)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
    );

    // Create Bundle for Work
    let bundle = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "feat/jwt-sign", "claims": "Added sign() function"}),
    );
    let bundle_id = bundle["id"].as_str().unwrap().to_string();
    assert_eq!(bundle["status"], "Proposed");

    // Verify full hierarchy in stores
    assert_eq!(stores.plans.read().unwrap().len(), 1);
    assert_eq!(stores.specs.read().unwrap().len(), 1);
    assert_eq!(stores.phases.read().unwrap().len(), 1);
    assert_eq!(stores.works.read().unwrap().len(), 1);
    assert_eq!(stores.bundles.read().unwrap().len(), 1);

    // Verify correct parent-child relationships
    let specs = stores.specs.read().unwrap();
    assert_eq!(specs[&spec_id].parent_id, plan_id);
    let phases = stores.phases.read().unwrap();
    assert_eq!(phases[&phase_id].parent_id, spec_id);
    let works = stores.works.read().unwrap();
    assert_eq!(works[&wi_id].parent_id, phase_id);
    let bundles = stores.bundles.read().unwrap();
    assert_eq!(bundles[&bundle_id].work_id, wi_id);
}

*/
/*
#[test]
fn test_bundle_lifecycle_via_dispatch() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create hierarchy so work.create can find a valid phase
    let (_plan_id, _spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

    // Create Work
    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "Task", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
    );
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // Create Bundle
    let bundle = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "feat/task", "claims": "Did it"}),
    );
    let bundle_id = bundle["id"].as_str().unwrap().to_string();

    // Proposed -> Triaged (Coordinator triages)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
    );

    // Triaged -> Reviewed (Reviewer reviews)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    );

    // Reviewed -> Accepted (Coordinator accepts)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target_status": "Accepted", "role": "coordinator"}),
    );

    // Verify final state
    let bundles = stores.bundles.read().unwrap();
    assert_eq!(
        bundles[&bundle_id].status(),
        crate::domain::bundle::BundleStatus::Accepted
    );
}
*/
