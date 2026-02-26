use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;

use crate::domain::plan::{Plan, PlanStatus, hierarchy_transitions};
use crate::domain::role::Role;
use crate::domain::spec::{Spec, SpecStatus};
use crate::domain::transition::validate_transition;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use super::context::Stores;

/// Dispatch an IPC request to the appropriate handler.
/// This is the central routing function for all daemon request handling.
pub fn dispatch(stores: &Arc<Stores>, event_tx: &broadcast::Sender<DaemonEvent>, req: DaemonRequest) -> DaemonResponse {
    match req.method.as_str() {
        "system.handshake" => handle_handshake(req),
        "plan.create" => handle_plan_create(stores, event_tx, req),
        "plan.get" => handle_plan_get(stores, req),
        "plan.list" => handle_plan_list(stores, req),
        "plan.transition" => handle_plan_transition(stores, event_tx, req),
        "spec.create" => handle_spec_create(stores, event_tx, req),
        "spec.get" => handle_spec_get(stores, req),
        "spec.list" => handle_spec_list(stores, req),
        "spec.transition" => handle_spec_transition(stores, event_tx, req),
        _ => DaemonResponse::err(req.id, RpcError::method_not_found(&req.method)),
    }
}

fn handle_handshake(req: DaemonRequest) -> DaemonResponse {
    DaemonResponse::ok(
        req.id,
        json!({
            "server_version": env!("CARGO_PKG_VERSION"),
            "protocol": "ndjson/1"
        }),
    )
}

fn handle_plan_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let title = req
        .params
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = req
        .params
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let acceptance_criteria = req
        .params
        .get("acceptance_criteria")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if title.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("title is required"));
    }

    let plan = Plan::new(title, description, acceptance_criteria);
    let plan_json = match serde_json::to_value(&plan) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = plan.id.clone();
    stores.plans.write().unwrap().insert(id.clone(), plan);
    let _ = event_tx.send(DaemonEvent::record_created("plan", &id));

    DaemonResponse::ok(req.id, plan_json)
}

fn handle_plan_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let plans = stores.plans.read().unwrap();
    match plans.get(id) {
        Some(plan) => match serde_json::to_value(plan) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("plan", id)),
    }
}

fn handle_plan_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let plans = stores.plans.read().unwrap();
    let plan_list: Vec<&Plan> = plans.values().collect();
    match serde_json::to_value(&plan_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_plan_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: PlanStatus = match req.params.get("target_status") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(s) => s,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid target_status")),
        },
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("target_status is required")),
    };

    let role: Role = match req.params.get("role") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid role")),
        },
        None => Role::Coordinator,
    };

    let mut plans = stores.plans.write().unwrap();
    let plan = match plans.get_mut(&id) {
        Some(p) => p,
        None => return DaemonResponse::err(req.id, RpcError::not_found("plan", &id)),
    };

    let from = plan.status;
    let rules = hierarchy_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    plan.status = target_status;
    plan.updated_at = crate::id::now_millis();

    let plan_json = match serde_json::to_value(&*plan) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "plan",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, plan_json)
}

// --- Spec handlers ---

fn handle_spec_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let plan_id = match req.params.get("plan_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("plan_id is required")),
    };

    // Verify parent plan exists
    if !stores.plans.read().unwrap().contains_key(&plan_id) {
        return DaemonResponse::err(req.id, RpcError::not_found("plan", &plan_id));
    }

    let title = req
        .params
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = req
        .params
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if title.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("title is required"));
    }

    let spec = Spec::new(plan_id, title, description);
    let spec_json = match serde_json::to_value(&spec) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = spec.id.clone();
    stores.specs.write().unwrap().insert(id.clone(), spec);
    let _ = event_tx.send(DaemonEvent::record_created("spec", &id));

    DaemonResponse::ok(req.id, spec_json)
}

fn handle_spec_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let specs = stores.specs.read().unwrap();
    match specs.get(id) {
        Some(spec) => match serde_json::to_value(spec) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("spec", id)),
    }
}

fn handle_spec_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let specs = stores.specs.read().unwrap();

    // Optionally filter by plan_id
    let plan_id_filter = req.params.get("plan_id").and_then(|v| v.as_str());

    let spec_list: Vec<&Spec> = specs
        .values()
        .filter(|s| plan_id_filter.is_none() || Some(s.plan_id.as_str()) == plan_id_filter)
        .collect();

    match serde_json::to_value(&spec_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_spec_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: SpecStatus = match req.params.get("target_status") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(s) => s,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid target_status")),
        },
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("target_status is required")),
    };

    let role: Role = match req.params.get("role") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid role")),
        },
        None => Role::Coordinator,
    };

    let mut specs = stores.specs.write().unwrap();
    let spec = match specs.get_mut(&id) {
        Some(s) => s,
        None => return DaemonResponse::err(req.id, RpcError::not_found("spec", &id)),
    };

    let from = spec.status;
    let rules = hierarchy_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    spec.status = target_status;
    spec.updated_at = crate::id::now_millis();

    let spec_json = match serde_json::to_value(&*spec) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "spec",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, spec_json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    fn test_event_tx() -> broadcast::Sender<DaemonEvent> {
        let (tx, _) = broadcast::channel(16);
        tx
    }

    // --- dispatch tests ---

    #[test]
    fn test_dispatch_unknown_method() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "unknown.method", json!(null));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unknown.method"));
    }

    #[test]
    fn test_dispatch_handshake() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "system.handshake", json!({"client_version": "0.1.0"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["protocol"], "ndjson/1");
    }

    // --- plan.create tests ---

    #[test]
    fn test_plan_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(
            1,
            "plan.create",
            json!({
                "title": "Test Plan",
                "description": "A test",
                "acceptance_criteria": "It works"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Plan");
        assert_eq!(result["status"], "draft");
        // Should be stored
        assert_eq!(stores.plans.read().unwrap().len(), 1);
    }

    #[test]
    fn test_plan_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "plan.create", json!({"description": "no title"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_plan_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let req = DaemonRequest::new(1, "plan.create", json!({"title": "Plan"}));
        dispatch(&stores, &tx, req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "plan");
    }

    // --- plan.get tests ---

    #[test]
    fn test_plan_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        // Create a plan first
        let create_req = DaemonRequest::new(1, "plan.create", json!({"title": "My Plan"}));
        let create_resp = dispatch(&stores, &tx, create_req);
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Get it
        let get_req = DaemonRequest::new(2, "plan.get", json!({"id": plan_id}));
        let get_resp = dispatch(&stores, &tx, get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My Plan");
    }

    #[test]
    fn test_plan_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "plan.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_plan_get_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "plan.get", json!({}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("id"));
    }

    // --- plan.list tests ---

    #[test]
    fn test_plan_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "plan.list", json!(null));
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        let plans = resp.result.unwrap();
        assert!(plans.is_array());
        assert_eq!(plans.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_plan_list_with_plans() {
        let stores = test_stores();
        let tx = test_event_tx();
        // Create two plans
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"})),
        );
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(2, "plan.create", json!({"title": "Plan B"})),
        );

        let req = DaemonRequest::new(3, "plan.list", json!(null));
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        let plans = resp.result.unwrap();
        assert_eq!(plans.as_array().unwrap().len(), 2);
    }

    // --- plan.transition tests ---

    #[test]
    fn test_plan_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();

        // Create plan
        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let _ = rx.try_recv(); // consume create event
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft → Active
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Check event was broadcast
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["from"], "Draft");
        assert_eq!(event.data["to"], "Active");
    }

    #[test]
    fn test_plan_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try to skip Draft → Complete (invalid: must go through Active)
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "complete",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_plan_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition plans
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_plan_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(
            1,
            "plan.transition",
            json!({
                "id": "nonexistent",
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_plan_transition_missing_params() {
        let stores = test_stores();
        let tx = test_event_tx();
        // Missing id
        let req = DaemonRequest::new(1, "plan.transition", json!({"target_status": "active"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());

        // Missing target_status
        let req = DaemonRequest::new(2, "plan.transition", json!({"id": "x"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
    }

    #[test]
    fn test_plan_transition_default_role_coordinator() {
        let stores = test_stores();
        let tx = test_event_tx();

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // No role specified — defaults to Coordinator, which is valid for hierarchy transitions
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    // --- spec.create tests ---

    /// Helper: create a plan and return its id
    fn create_test_plan(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>) -> String {
        let resp = dispatch(
            stores,
            tx,
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_spec_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let plan_id = create_test_plan(&stores, &tx);

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({
                "plan_id": plan_id,
                "title": "Test Spec",
                "description": "A spec"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Spec");
        assert_eq!(result["plan_id"], plan_id);
        assert_eq!(result["status"], "draft");
        assert_eq!(stores.specs.read().unwrap().len(), 1);
    }

    #[test]
    fn test_spec_create_missing_plan_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "spec.create", json!({"title": "Spec"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("plan_id"));
    }

    #[test]
    fn test_spec_create_plan_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(
            1,
            "spec.create",
            json!({"plan_id": "nonexistent", "title": "Spec"}),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_spec_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let plan_id = create_test_plan(&stores, &tx);
        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"plan_id": plan_id, "description": "no title"}),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_spec_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let plan_id = create_test_plan(&stores, &tx);
        let _ = rx.try_recv(); // consume plan create event

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Spec"}),
        );
        dispatch(&stores, &tx, req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "spec");
    }

    // --- spec.get tests ---

    #[test]
    fn test_spec_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let plan_id = create_test_plan(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "My Spec"})),
        );
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(3, "spec.get", json!({"id": spec_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My Spec");
    }

    #[test]
    fn test_spec_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "spec.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    // --- spec.list tests ---

    #[test]
    fn test_spec_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "spec.list", json!(null));
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_spec_list_filtered_by_plan_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let plan_id_1 = create_test_plan(&stores, &tx);

        // Create a second plan
        let resp2 = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(10, "plan.create", json!({"title": "Plan 2"})),
        );
        let plan_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create specs under different plans
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id_1, "title": "Spec A"})),
        );
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(3, "spec.create", json!({"plan_id": plan_id_2, "title": "Spec B"})),
        );

        // List all — should have 2
        let all_resp = dispatch(&stores, &tx, DaemonRequest::new(4, "spec.list", json!(null)));
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by plan_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(5, "spec.list", json!({"plan_id": plan_id_1})),
        );
        let specs = filtered_resp.result.unwrap();
        let arr = specs.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Spec A");
    }

    // --- spec.transition tests ---

    #[test]
    fn test_spec_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let plan_id = create_test_plan(&stores, &tx);
        let _ = rx.try_recv(); // consume plan create event

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"})),
        );
        let _ = rx.try_recv(); // consume spec create event
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "spec");
        assert_eq!(event.data["from"], "Draft");
        assert_eq!(event.data["to"], "Active");
    }

    #[test]
    fn test_spec_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let plan_id = create_test_plan(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"})),
        );
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "complete",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_spec_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let plan_id = create_test_plan(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"})),
        );
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "active",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_spec_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(
            1,
            "spec.transition",
            json!({
                "id": "nonexistent",
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }
}
