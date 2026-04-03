use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;

use crate::daemon::context::Stores;
use crate::daemon::handlers::dispatch;
use crate::daemon::handlers::tests::{
    test_event_tx, test_integrator_config, test_stores, test_stores_with_taskstore, test_worktree_mgr,
};
use crate::domain::bundle::Bundle;
use crate::domain::tick::{Tick, TickStatus};
use crate::domain::work::WorkStatus;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
use crate::worktree::manager::WorktreeManager;

/// Helper: create plan + spec + phase and return (plan_id, spec_id, phase_id)
fn create_test_phase(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
) -> (String, String, String) {
    let plan_resp = dispatch(
        stores,
        tx,
        wm,
        &test_integrator_config(),
        DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
    );
    let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();
    let spec_resp = dispatch(
        stores,
        tx,
        wm,
        &test_integrator_config(),
        DaemonRequest::new(10, "spec.create", json!({"plan_id": plan_id, "title": "Parent Spec"})),
    );
    let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();
    let phase_resp = dispatch(
        stores,
        tx,
        wm,
        &test_integrator_config(),
        DaemonRequest::new(
            20,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Parent Phase", "order": 1}),
        ),
    );
    let phase_id = phase_resp.result.unwrap()["id"].as_str().unwrap().to_string();
    (plan_id, spec_id, phase_id)
}

/// Helper: create plan + spec + phase + work and return (phase_id, work_id)
fn create_test_work(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
) -> (String, String) {
    let (_, _, phase_id) = create_test_phase(stores, tx, wm);
    let resp = dispatch(
        stores,
        tx,
        wm,
        &test_integrator_config(),
        DaemonRequest::new(
            30,
            "work.create",
            json!({"phase_id": phase_id, "title": "Parent WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        ),
    );
    let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
    (phase_id, wi_id)
}

/// Helper: create plan + spec + phase + work + bundle and return (work_id, bundle_id)
fn create_test_bundle(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
) -> (String, String) {
    let (_, wi_id) = create_test_work(stores, tx, wm);
    let resp = dispatch(
        stores,
        tx,
        wm,
        &test_integrator_config(),
        DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/test", "base_tick_id": null, "claims": "Initial claims"}),
        ),
    );
    assert!(!resp.is_error(), "bundle.create failed: {:?}", resp.error);
    let bundle_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
    (wi_id, bundle_id)
}

/// Helper: insert a Published Tick into the store and return its ID.
fn insert_published_tick(stores: &Arc<Stores>, number: u32) -> String {
    let mut tick = Tick::new(number);
    tick.force_status(TickStatus::Published);
    tick.integration_sha = Some(format!("sha-{number}"));
    let id = tick.id.clone();
    stores.ticks.write().unwrap().insert(id.clone(), tick);
    id
}

// === Tests from mod.rs lines 1282-1338 ===

#[test]
fn test_bundle_create_rejects_done_work() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    // Directly set work status to Done via the HashMap (bypasses transition preconditions)
    {
        let mut works = stores.works.write().unwrap();
        let work = works.get_mut(&wi_id).unwrap();
        work.force_status(WorkStatus::Done);
    }

    let req = DaemonRequest::new(
        2,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "feature/late"}),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert!(resp.error.unwrap().message.contains("Done work"));
}

#[test]
fn test_bundle_create_rejects_abandoned_work() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    // Transition work: Ready -> Abandoned
    dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            1,
            "work.transition",
            json!({"id": wi_id, "target_status": "Abandoned", "role": "coordinator"}),
        ),
    );

    let req = DaemonRequest::new(
        2,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "feature/abandoned"}),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert!(resp.error.unwrap().message.contains("Abandoned work"));
}

// === Tests from mod.rs lines 2860-3517 ===

#[test]
fn test_bundle_create_persists_to_taskstore() {
    let (_dir, stores) = test_stores_with_taskstore();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "branch_name": "feature/persist",
            "base_tick_id": "tick-001",
            "claims": "Persisted bundle"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(!resp.is_error());
    let bundle_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

    // Verify it was persisted to TaskStore
    let store = stores.store.as_ref().unwrap().lock().unwrap();
    let retrieved: Option<Bundle> = store.get(&bundle_id).unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().claims, vec!["Persisted bundle".to_string()]);
}

#[test]
fn test_bundle_create_success() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "branch_name": "feature/auth",
            "base_tick_id": "tick-001",
            "claims": "Add JWT signing"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(!resp.is_error());
    let result = resp.result.unwrap();
    assert_eq!(result["work_id"], wi_id);
    assert_eq!(result["branch_name"], "feature/auth");
    assert_eq!(result["base_tick_id"], "tick-001");
    assert_eq!(result["claims"], serde_json::json!(["Add JWT signing"]));
    assert_eq!(result["status"], "Proposed");
    assert_eq!(stores.bundles.read().unwrap().len(), 1);
}

#[test]
fn test_bundle_create_no_base_tick() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "branch_name": "feature/init"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(!resp.is_error());
    let result = resp.result.unwrap();
    assert!(result["base_tick_id"].is_null());
}

#[test]
fn test_bundle_create_missing_work_id() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let req = DaemonRequest::new(1, "bundle.create", json!({"branch_name": "feature/x"}));
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert!(resp.error.unwrap().message.contains("work_id"));
}

#[test]
fn test_bundle_create_work_not_found() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let req = DaemonRequest::new(
        1,
        "bundle.create",
        json!({"work_id": "nonexistent", "branch_name": "feature/x"}),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert_eq!(resp.error.unwrap().code, -32001);
}

#[test]
fn test_bundle_create_missing_branch_name() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    let req = DaemonRequest::new(40, "bundle.create", json!({"work_id": wi_id, "claims": "stuff"}));
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert!(resp.error.unwrap().message.contains("branch_name"));
}

#[test]
fn test_bundle_create_broadcasts_event() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let mut rx = tx.subscribe();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    // Drain plan+spec+phase+work create events
    let _ = rx.try_recv();
    let _ = rx.try_recv();
    let _ = rx.try_recv();
    let _ = rx.try_recv();

    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({"work_id": wi_id, "branch_name": "feature/x"}),
    );
    dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    let event = rx.try_recv().unwrap();
    assert_eq!(event.event, "record.created");
    assert_eq!(event.data["collection"], "bundle");
}

#[test]
fn test_bundle_get_success() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    let create_resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/auth"}),
        ),
    );
    let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

    let get_resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(41, "bundle.get", json!({"id": bundle_id})),
    );
    assert!(!get_resp.is_error());
    assert_eq!(get_resp.result.unwrap()["branch_name"], "feature/auth");
}

#[test]
fn test_bundle_get_not_found() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let req = DaemonRequest::new(1, "bundle.get", json!({"id": "nonexistent"}));
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert_eq!(resp.error.unwrap().code, -32001);
}

#[test]
fn test_bundle_get_reads_from_taskstore() {
    let (_dir, stores) = test_stores_with_taskstore();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    // Create a bundle (writes to both TaskStore and HashMap)
    let create_resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/ts-read"}),
        ),
    );
    assert!(!create_resp.is_error());
    let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

    // Remove from HashMap to prove get reads from TaskStore
    stores.bundles.write().unwrap().remove(&bundle_id);

    // Get should still succeed via TaskStore
    let get_req = DaemonRequest::new(41, "bundle.get", json!({"id": bundle_id}));
    let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
    assert!(!get_resp.is_error());
    assert_eq!(get_resp.result.unwrap()["branch_name"], "feature/ts-read");
}

#[test]
fn test_bundle_list_empty() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let req = DaemonRequest::new(1, "bundle.list", json!(null));
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(!resp.is_error());
    assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
}

#[test]
fn test_bundle_list_filtered_by_work_id() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (phase_id, wi_id_1) = create_test_work(&stores, &tx, &wm);

    // Create a second work item under the same phase
    let resp2 = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            31,
            "work.create",
            json!({"phase_id": phase_id, "title": "WI 2", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        ),
    );
    let wi_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

    // Create bundles under different work items
    dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id_1, "branch_name": "feature/a"}),
        ),
    );
    dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            41,
            "bundle.create",
            json!({"work_id": wi_id_2, "branch_name": "feature/b"}),
        ),
    );

    // List all - should have 2
    let all_resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(50, "bundle.list", json!(null)),
    );
    assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

    // List filtered by wi_id_1 - should have 1
    let filtered_resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(51, "bundle.list", json!({"work_id": wi_id_1})),
    );
    let bundles = filtered_resp.result.unwrap();
    let arr = bundles.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["branch_name"], "feature/a");
}

#[test]
fn test_bundle_list_reads_from_taskstore() {
    let (_dir, stores) = test_stores_with_taskstore();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (phase_id, wi_id_1) = create_test_work(&stores, &tx, &wm);

    // Create a second work item under the same phase
    let resp2 = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            31,
            "work.create",
            json!({"phase_id": phase_id, "title": "WI 2", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        ),
    );
    let wi_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

    // Create bundles under different work items (writes to both TaskStore and HashMap)
    dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id_1, "branch_name": "feature/a"}),
        ),
    );
    dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            41,
            "bundle.create",
            json!({"work_id": wi_id_2, "branch_name": "feature/b"}),
        ),
    );

    // Clear HashMap to prove list reads from TaskStore
    stores.bundles.write().unwrap().clear();

    // List all should still return both bundles via TaskStore
    let all_resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(50, "bundle.list", json!(null)),
    );
    assert!(!all_resp.is_error());
    assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

    // Test filtered list also works from TaskStore
    let filtered_req = DaemonRequest::new(51, "bundle.list", json!({"work_id": wi_id_1}));
    let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
    assert!(!filtered_resp.is_error());
    let filtered_items = filtered_resp.result.unwrap();
    let arr = filtered_items.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["branch_name"], "feature/a");
}

#[test]
fn test_bundle_transition_proposed_to_triaged() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let mut rx = tx.subscribe();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    // Drain plan+spec+phase+work create events
    let _ = rx.try_recv();
    let _ = rx.try_recv();
    let _ = rx.try_recv();
    let _ = rx.try_recv();

    let create_resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/x"}),
        ),
    );
    let _ = rx.try_recv(); // consume bundle create event
    let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

    let req = DaemonRequest::new(
        41,
        "bundle.transition",
        json!({
            "id": bundle_id,
            "target_status": "Triaged",
            "role": "coordinator"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(!resp.is_error());
    assert_eq!(resp.result.unwrap()["status"], "Triaged");

    let event = rx.try_recv().unwrap();
    assert_eq!(event.event, "transition.completed");
    assert_eq!(event.data["collection"], "bundle");
    assert_eq!(event.data["from"], "Proposed");
    assert_eq!(event.data["to"], "Triaged");
}

#[test]
fn test_bundle_transition_invalid_skip_state() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    let create_resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/x"}),
        ),
    );
    let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

    // Try Proposed -> Accepted (invalid: must go through Triaged -> Reviewed)
    let req = DaemonRequest::new(
        41,
        "bundle.transition",
        json!({
            "id": bundle_id,
            "target_status": "Accepted",
            "role": "coordinator"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert_eq!(resp.error.unwrap().code, -32000);
}

#[test]
fn test_bundle_transition_wrong_role() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    let create_resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/x"}),
        ),
    );
    let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

    // Implementer cannot transition Proposed -> Triaged
    let req = DaemonRequest::new(
        41,
        "bundle.transition",
        json!({
            "id": bundle_id,
            "target_status": "Triaged",
            "role": "implementer"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert_eq!(resp.error.unwrap().code, -32000);
}

#[test]
fn test_bundle_transition_not_found() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let req = DaemonRequest::new(
        1,
        "bundle.transition",
        json!({
            "id": "nonexistent",
            "target_status": "Triaged"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert_eq!(resp.error.unwrap().code, -32001);
}

// --- Staleness guard tests ---

#[test]
fn test_bundle_create_staleness_guard_rejects_no_base_tick_when_published_exists() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    let _ = insert_published_tick(&stores, 1);

    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "branch_name": "feature/auth"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32002);
    assert!(err.message.contains("staleness guard"));
}

#[test]
fn test_bundle_create_staleness_guard_rejects_stale_base_tick() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    let _ = insert_published_tick(&stores, 1);
    let latest_tick_id = insert_published_tick(&stores, 2);

    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "branch_name": "feature/auth",
            "base_tick_id": "old-stale-tick-id"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32002);
    assert!(err.message.contains("staleness guard"));
    assert!(err.message.contains(&latest_tick_id));
}

#[test]
fn test_bundle_create_staleness_guard_accepts_matching_base_tick() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    let tick_id = insert_published_tick(&stores, 1);

    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "branch_name": "feature/auth",
            "base_tick_id": tick_id,
            "claims": "Add auth"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(!resp.is_error(), "Expected success but got: {:?}", resp.error);
    let result = resp.result.unwrap();
    assert_eq!(result["base_tick_id"], tick_id);
    assert_eq!(result["status"], "Proposed");
}

#[test]
fn test_bundle_create_staleness_guard_uses_highest_tick_number() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    let tick1_id = insert_published_tick(&stores, 1);
    let tick2_id = insert_published_tick(&stores, 2);

    // Using tick1's ID should be rejected (tick2 is latest)
    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "branch_name": "feature/auth",
            "base_tick_id": tick1_id,
            "claims": "Add auth"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(resp.is_error());
    assert!(resp.error.unwrap().message.contains(&tick2_id));
}

#[test]
fn test_bundle_create_staleness_guard_broadcasts_stale_event() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let mut rx = tx.subscribe();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    // Drain create events
    while rx.try_recv().is_ok() {}

    let _ = insert_published_tick(&stores, 1);

    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "branch_name": "feature/auth",
            "base_tick_id": "stale-id"
        }),
    );
    dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    let event = rx.try_recv().unwrap();
    assert_eq!(event.event, "bundle.rejected_stale");
    assert_eq!(event.data["bundle_work_id"], wi_id.as_str());
    assert_eq!(event.data["base_tick_id"], "stale-id");
}

#[test]
fn test_bundle_create_bootstrap_no_published_tick_no_base() {
    // Bootstrap case: no published tick, no base_tick_id -> OK
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);

    let req = DaemonRequest::new(
        40,
        "bundle.create",
        json!({
            "work_id": wi_id,
            "branch_name": "feature/init"
        }),
    );
    let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
    assert!(!resp.is_error());
}

// === Tests from mod.rs lines 7640-7777 ===

#[test]
fn test_handle_bundle_update_success() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, bundle_id) = create_test_bundle(&stores, &tx, &wm);

    let resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            2,
            "bundle.update",
            json!({
                "id": bundle_id,
                "description": "Updated desc",
                "verification": "tests pass",
                "locks_used": ["lock-1"],
                "base_tick_id": "tick-002"
            }),
        ),
    );
    assert!(!resp.is_error(), "bundle.update failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    assert_eq!(result["description"], "Updated desc");
    assert_eq!(result["verification"], "tests pass");
    assert_eq!(result["locks_used"].as_array().unwrap().len(), 1);
    assert_eq!(result["base_tick_id"], "tick-002");
}

#[test]
fn test_handle_bundle_update_not_found() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(1, "bundle.update", json!({"id": "nonexistent", "description": "x"})),
    );
    assert!(resp.is_error());
}

#[test]
fn test_handle_bundle_update_missing_id() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(1, "bundle.update", json!({"description": "x"})),
    );
    assert!(resp.is_error());
}

#[test]
fn test_handle_bundle_update_size_policy_rejects_too_many_files() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, bundle_id) = create_test_bundle(&stores, &tx, &wm);

    // Default max_files_touched is 8, so 9 paths should be rejected
    let too_many_paths: Vec<String> = (0..9).map(|i| format!("file_{}.rs", i)).collect();
    let resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            2,
            "bundle.update",
            json!({
                "id": bundle_id,
                "touched_paths": too_many_paths
            }),
        ),
    );
    assert!(resp.is_error(), "expected size policy rejection but got success");
}

#[test]
fn test_handle_bundle_update_claims_string_backward_compat() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, bundle_id) = create_test_bundle(&stores, &tx, &wm);

    let resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            2,
            "bundle.update",
            json!({"id": bundle_id, "claims": "single claim string"}),
        ),
    );
    assert!(!resp.is_error(), "bundle.update claims string failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    let claims = result["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0], "single claim string");
}

#[test]
fn test_handle_bundle_update_claims_array() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, bundle_id) = create_test_bundle(&stores, &tx, &wm);

    let resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            2,
            "bundle.update",
            json!({"id": bundle_id, "claims": ["claim 1", "claim 2"]}),
        ),
    );
    assert!(!resp.is_error(), "bundle.update claims array failed: {:?}", resp.error);
    let result = resp.result.unwrap();
    let claims = result["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0], "claim 1");
    assert_eq!(claims[1], "claim 2");
}

#[test]
fn test_handle_bundle_create_rejects_too_many_loc() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    // Default max_loc_changed is 300, so 301 should be rejected
    let resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            1,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feat/test",
                "claims": ["test claim"],
                "loc_changed": 301
            }),
        ),
    );
    assert!(resp.is_error(), "expected loc policy rejection");
}

#[test]
fn test_handle_bundle_create_accepts_loc_within_limit() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, wi_id) = create_test_work(&stores, &tx, &wm);
    let resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            1,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feat/test",
                "claims": ["test claim"],
                "loc_changed": 300
            }),
        ),
    );
    assert!(!resp.is_error(), "loc within limit should succeed: {:?}", resp.error);
}

#[test]
fn test_handle_bundle_update_rejects_too_many_loc() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let (_, bundle_id) = create_test_bundle(&stores, &tx, &wm);

    let resp = dispatch(
        &stores,
        &tx,
        &wm,
        &test_integrator_config(),
        DaemonRequest::new(
            2,
            "bundle.update",
            json!({
                "id": bundle_id,
                "loc_changed": 301
            }),
        ),
    );
    assert!(resp.is_error(), "expected loc policy rejection on update");
}
