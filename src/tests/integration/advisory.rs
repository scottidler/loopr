#![allow(clippy::unwrap_used)]

use serde_json::json;

use crate::config::IntegratorConfig;
use crate::daemon::handlers::dispatch;
use crate::domain::learning::{Learning, LearningScope};
use crate::fsm::runtime::FsmInterpreter;
use crate::ipc::protocol::DaemonRequest;
use crate::test_util::TestDir;

use super::fixtures::*;

#[tokio::test]
async fn test_advisory_review_bundle_accepted_directly() {
    // Verify that the Coordinator can accept a Bundle directly (Triaged->Accepted)
    // without waiting for Reviewer verdict (the Integrator is the hard gate).
    let dir = TestDir::new("loopr-int-advisory");
    let stores = test_stores_with_persistence(&dir);
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = IntegratorConfig::default();

    // Create hierarchy: plan -> spec -> phase -> work
    let plan_resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "Advisory Test", "description": "Test advisory review", "acceptance-criteria": "tests pass"}),
    )
    .await;
    let plan_id = plan_resp["id"].as_str().unwrap();
    let spec_resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent-id": plan_id, "title": "Spec", "description": "spec"}),
    )
    .await;
    let spec_id = spec_resp["id"].as_str().unwrap();
    let phase_resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent-id": spec_id, "title": "Phase", "description": "phase", "order": 1}),
    )
    .await;
    let phase_id = phase_resp["id"].as_str().unwrap();
    let work_resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent-id": phase_id, "title": "Work", "description": "work", "files": ["src/main.rs"]}),
    )
    .await;
    let work_id = work_resp["id"].as_str().unwrap();

    // Create a bundle
    {
        use crate::domain::bundle::{Bundle, BundleStatus};
        let mut bundle = Bundle::new(
            work_id.to_string(),
            None,
            "feature/test".to_string(),
            vec!["test claim".to_string()],
        );
        bundle.force_status(BundleStatus::Proposed);
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);
    }

    let bundle_id = stores.bundles.read().unwrap().keys().next().unwrap().clone();

    // Coordinator triages: Proposed -> Triaged
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": &bundle_id, "target-status": "Triaged", "role": "coordinator"}),
    )
    .await;
    assert_eq!(
        stores.bundles.read().unwrap()[&bundle_id].status(),
        crate::domain::bundle::BundleStatus::Triaged
    );

    // Coordinator accepts directly: Triaged -> Accepted (bypassing review)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": &bundle_id, "target-status": "Accepted", "role": "coordinator", "verification": "Coordinator direct accept"}),
    )
    .await;
    assert_eq!(
        stores.bundles.read().unwrap()[&bundle_id].status(),
        crate::domain::bundle::BundleStatus::Accepted
    );
}

#[test]
fn test_reviewer_feedback_learning_available_after_advisory_accept() {
    // When the Coordinator accepts directly and the Reviewer creates feedback,
    // the feedback Learning should be available in stores for future iterations.
    let stores = test_stores();

    // Create a review feedback Learning (simulating what the Reviewer would create)
    let learning = Learning::new(
        "work-1".to_string(),
        LearningScope::Work,
        "Review feedback (approve): Clean code, well tested".to_string(),
    );
    let learning_id = learning.id.clone();
    stores.learnings.write().unwrap().insert(learning.id.clone(), learning);

    // Verify the Learning is accessible
    let learnings = stores.learnings.read().unwrap();
    let feedback = learnings.get(&learning_id).unwrap();
    assert!(feedback.content.contains("Review feedback"));
    assert!(feedback.content.contains("approve"));
}

#[tokio::test]
async fn test_advisory_bypass_rejected_for_non_coordinator_via_dispatch() {
    // Verify that only the Coordinator can use Triaged->Accepted through the IPC handler.
    let dir = TestDir::new("loopr-int-advisory-reject");
    let stores = test_stores_with_persistence(&dir);
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = IntegratorConfig::default();

    // Create hierarchy
    let plan_resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "T", "description": "d", "acceptance-criteria": "c"}),
    )
    .await;
    let plan_id = plan_resp["id"].as_str().unwrap();
    let spec_resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent-id": plan_id, "title": "S", "description": "d"}),
    )
    .await;
    let spec_id = spec_resp["id"].as_str().unwrap();
    let phase_resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent-id": spec_id, "title": "P", "description": "d", "order": 1}),
    )
    .await;
    let phase_id = phase_resp["id"].as_str().unwrap();
    let work_resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent-id": phase_id, "title": "W", "description": "d", "files": ["src/x.rs"]}),
    )
    .await;
    let work_id = work_resp["id"].as_str().unwrap();

    // Create and triage a bundle
    {
        use crate::domain::bundle::{Bundle, BundleStatus};
        let mut bundle = Bundle::new(work_id.to_string(), None, "f/t".to_string(), vec!["claim".to_string()]);
        bundle.force_status(BundleStatus::Proposed);
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);
    }
    let bundle_id = stores.bundles.read().unwrap().keys().next().unwrap().clone();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": &bundle_id, "target-status": "Triaged", "role": "coordinator"}),
    )
    .await;

    let fsm = FsmInterpreter::embedded().unwrap();

    // Reviewer trying Triaged->Accepted should FAIL
    let req = DaemonRequest::new(
        1,
        "bundle.transition",
        json!({"id": &bundle_id, "target-status": "Accepted", "role": "reviewer", "verification": "v"}),
    );
    let resp = dispatch(&stores, &tx, &wm, &ic, &fsm, req).await;
    assert!(resp.is_error(), "Reviewer should not be able to use Triaged->Accepted");

    // Implementer trying Triaged->Accepted should FAIL
    let req = DaemonRequest::new(
        2,
        "bundle.transition",
        json!({"id": &bundle_id, "target-status": "Accepted", "role": "implementer", "verification": "v"}),
    );
    let resp = dispatch(&stores, &tx, &wm, &ic, &fsm, req).await;
    assert!(
        resp.is_error(),
        "Implementer should not be able to use Triaged->Accepted"
    );

    // Coordinator trying Triaged->Accepted should SUCCEED
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": &bundle_id, "target-status": "Accepted", "role": "coordinator", "verification": "Coordinator direct"}),
    )
    .await;
    assert_eq!(
        stores.bundles.read().unwrap()[&bundle_id].status(),
        crate::domain::bundle::BundleStatus::Accepted
    );
}
