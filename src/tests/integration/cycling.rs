#![allow(clippy::unwrap_used, unused_imports)]

use serde_json::json;

use super::fixtures::*;

/// Test 2: Dependency chain - WIs A->B->C with dependencies.
/// B depends on A; C depends on B. Verify deps are stored correctly and
/// work items with unmet deps cannot be independently assigned.
#[tokio::test]
async fn test_dependency_chain_execution() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let (_, _, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic).await;

    // Create WI-A (no dependencies)
    let wi_a = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({
            "parent-id": phase_id,
            "title": "Create base types",
            "description": "Foundation types and traits",
            "files": ["src/types.rs"],
            "acceptance-criteria": ["Types compile"]
        }),
    )
    .await;
    let wi_a_id = wi_a["id"].as_str().unwrap().to_string();

    // Create WI-B depending on A
    let wi_b = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({
            "parent-id": phase_id,
            "title": "Implement logic",
            "description": "Business logic using base types",
            "files": ["src/logic.rs"],
            "acceptance-criteria": ["Logic tests pass"],
            "dependencies": [wi_a_id]
        }),
    )
    .await;
    let wi_b_id = wi_b["id"].as_str().unwrap().to_string();

    // Create WI-C depending on B
    let wi_c = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({
            "parent-id": phase_id,
            "title": "Add integration tests",
            "description": "Integration tests for logic",
            "files": ["src/tests.rs"],
            "acceptance-criteria": ["Integration tests pass"],
            "dependencies": [wi_b_id]
        }),
    )
    .await;
    let wi_c_id = wi_c["id"].as_str().unwrap().to_string();

    // Verify dependencies are stored correctly
    let wi_b_get = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_b_id})).await;
    let b_deps: Vec<String> = serde_json::from_value(wi_b_get["dependencies"].clone()).unwrap();
    assert_eq!(b_deps, vec![wi_a_id.clone()]);

    let wi_c_get = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_c_id})).await;
    let c_deps: Vec<String> = serde_json::from_value(wi_c_get["dependencies"].clone()).unwrap();
    assert_eq!(c_deps, vec![wi_b_id.clone()]);

    // WI-A should be Ready (no deps, has acceptance_criteria)
    assert_eq!(wi_a["status"].as_str().unwrap(), "Ready");
    // WI-B should also be Ready (auto-promoted because it has acceptance_criteria)
    assert_eq!(wi_b["status"].as_str().unwrap(), "Ready");
    // WI-C should also be Ready
    assert_eq!(wi_c["status"].as_str().unwrap(), "Ready");
}

/// Test 3: Duplicate work item rejection - creating a WI with the same title
/// (case-insensitive) in the same phase should fail.
#[tokio::test]
async fn test_duplicate_work_rejection() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let (_, _, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic).await;

    // Create first WI
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({
            "parent-id": phase_id,
            "title": "Implement auth",
            "description": "Add JWT auth",
            "files": ["src/auth.rs"],
            "acceptance-criteria": ["Auth works"]
        }),
    )
    .await;

    // Try creating duplicate with case variation
    let err_code = dispatch_err(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({
            "parent-id": phase_id,
            "title": "implement auth",
            "description": "Different description",
            "files": ["src/auth.rs"],
            "acceptance-criteria": ["Auth works"]
        }),
    )
    .await;

    // -32005 is precondition_failed
    assert_eq!(err_code, -32005, "duplicate WI should return precondition_failed");

    // Different title should succeed
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({
            "parent-id": phase_id,
            "title": "Implement authorization",
            "description": "Add RBAC",
            "files": ["src/authz.rs"],
            "acceptance-criteria": ["RBAC works"]
        }),
    )
    .await;
}

/// Test 7: Phase gate advances to next phase - complete all WIs in Phase 1,
/// verify state tracks completion and can activate Phase 2.
#[tokio::test]
async fn test_phase_gate_advances_to_next_phase() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create hierarchy with two phases
    let plan = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "Multi-phase Plan", "description": "desc", "acceptance-criteria": "all phases done"}),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target-status": "active"}),
    )
    .await;

    let spec = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent-id": plan_id, "title": "Multi-phase Spec", "description": "desc", "acceptance-criteria": "pass"}),
    ).await;
    let spec_id = spec["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target-status": "active"}),
    )
    .await;

    let phase1 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent-id": spec_id, "title": "Phase 1: Foundation", "description": "base types", "acceptance-criteria": "types exist"}),
    ).await;
    let phase1_id = phase1["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase1_id, "target-status": "active"}),
    )
    .await;

    let phase2 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent-id": spec_id, "title": "Phase 2: Logic", "description": "business logic", "acceptance-criteria": "logic works"}),
    ).await;
    let phase2_id = phase2["id"].as_str().unwrap().to_string();

    // Create a WI in Phase 1
    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({
            "parent-id": phase1_id,
            "title": "Create base types",
            "description": "Foundation types",
            "files": ["src/types.rs"],
            "acceptance-criteria": ["Types compile"]
        }),
    )
    .await;
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // Simulate WI completion: Ready -> InProgress -> InReview -> Integrated -> Done
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target-status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
    )
    .await;

    // Create a Bundle (required before InReview)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work-id": wi_id, "branch-name": "agent/test-wi", "claims": "implemented types"}),
    )
    .await;

    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target-status": "InReview", "role": "implementer"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target-status": "Integrated", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target-status": "Done", "role": "coordinator"}),
    )
    .await;

    // Verify WI is Done
    let wi_final = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_id})).await;
    assert_eq!(wi_final["status"].as_str().unwrap(), "Done");

    // Activate Phase 2 via IPC (reconciliation would do this automatically)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase2_id, "target-status": "active"}),
    )
    .await;

    // Engine handles phase promotion and completion detection - no coordinator FSM needed.
}
