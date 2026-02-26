use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;

use crate::domain::bundle::{Bundle, BundleStatus, bundle_transitions};
use crate::domain::phase::{Phase, PhaseStatus};
use crate::domain::plan::{Plan, PlanStatus, hierarchy_transitions};
use crate::domain::role::Role;
use crate::domain::spec::{Spec, SpecStatus};
use crate::domain::transition::validate_transition;
use crate::domain::work_item::{WorkItem, WorkItemStatus, work_item_transitions};
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
        "phase.create" => handle_phase_create(stores, event_tx, req),
        "phase.get" => handle_phase_get(stores, req),
        "phase.list" => handle_phase_list(stores, req),
        "phase.transition" => handle_phase_transition(stores, event_tx, req),
        "work_item.create" => handle_work_item_create(stores, event_tx, req),
        "work_item.get" => handle_work_item_get(stores, req),
        "work_item.list" => handle_work_item_list(stores, req),
        "work_item.transition" => handle_work_item_transition(stores, event_tx, req),
        "bundle.create" => handle_bundle_create(stores, event_tx, req),
        "bundle.get" => handle_bundle_get(stores, req),
        "bundle.list" => handle_bundle_list(stores, req),
        "bundle.transition" => handle_bundle_transition(stores, event_tx, req),
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

// --- Phase handlers ---

fn handle_phase_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let spec_id = match req.params.get("spec_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("spec_id is required")),
    };

    // Verify parent spec exists
    if !stores.specs.read().unwrap().contains_key(&spec_id) {
        return DaemonResponse::err(req.id, RpcError::not_found("spec", &spec_id));
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
    let order = req.params.get("order").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    if title.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("title is required"));
    }

    let phase = Phase::new(spec_id, title, description, order);
    let phase_json = match serde_json::to_value(&phase) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = phase.id.clone();
    stores.phases.write().unwrap().insert(id.clone(), phase);
    let _ = event_tx.send(DaemonEvent::record_created("phase", &id));

    DaemonResponse::ok(req.id, phase_json)
}

fn handle_phase_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let phases = stores.phases.read().unwrap();
    match phases.get(id) {
        Some(phase) => match serde_json::to_value(phase) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("phase", id)),
    }
}

fn handle_phase_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let phases = stores.phases.read().unwrap();

    // Optionally filter by spec_id
    let spec_id_filter = req.params.get("spec_id").and_then(|v| v.as_str());

    let phase_list: Vec<&Phase> = phases
        .values()
        .filter(|p| spec_id_filter.is_none() || Some(p.spec_id.as_str()) == spec_id_filter)
        .collect();

    match serde_json::to_value(&phase_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_phase_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: PhaseStatus = match req.params.get("target_status") {
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

    let mut phases = stores.phases.write().unwrap();
    let phase = match phases.get_mut(&id) {
        Some(p) => p,
        None => return DaemonResponse::err(req.id, RpcError::not_found("phase", &id)),
    };

    let from = phase.status;
    let rules = hierarchy_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    phase.status = target_status;
    phase.updated_at = crate::id::now_millis();

    let phase_json = match serde_json::to_value(&*phase) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "phase",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, phase_json)
}

// --- WorkItem handlers ---

fn handle_work_item_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let phase_id = match req.params.get("phase_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("phase_id is required")),
    };

    // Verify parent phase exists
    if !stores.phases.read().unwrap().contains_key(&phase_id) {
        return DaemonResponse::err(req.id, RpcError::not_found("phase", &phase_id));
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

    let work_item = WorkItem::new(phase_id, title, description);
    let wi_json = match serde_json::to_value(&work_item) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = work_item.id.clone();
    stores.work_items.write().unwrap().insert(id.clone(), work_item);
    let _ = event_tx.send(DaemonEvent::record_created("work_item", &id));

    DaemonResponse::ok(req.id, wi_json)
}

fn handle_work_item_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let work_items = stores.work_items.read().unwrap();
    match work_items.get(id) {
        Some(wi) => match serde_json::to_value(wi) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("work_item", id)),
    }
}

fn handle_work_item_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let work_items = stores.work_items.read().unwrap();

    // Optionally filter by phase_id
    let phase_id_filter = req.params.get("phase_id").and_then(|v| v.as_str());

    let wi_list: Vec<&WorkItem> = work_items
        .values()
        .filter(|wi| phase_id_filter.is_none() || Some(wi.phase_id.as_str()) == phase_id_filter)
        .collect();

    match serde_json::to_value(&wi_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_work_item_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: WorkItemStatus = match req.params.get("target_status") {
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

    let mut work_items = stores.work_items.write().unwrap();
    let wi = match work_items.get_mut(&id) {
        Some(w) => w,
        None => return DaemonResponse::err(req.id, RpcError::not_found("work_item", &id)),
    };

    let from = wi.status;
    let rules = work_item_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    wi.status = target_status;
    wi.updated_at = crate::id::now_millis();

    let wi_json = match serde_json::to_value(&*wi) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "work_item",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, wi_json)
}

// --- Bundle handlers ---

fn handle_bundle_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let work_item_id = match req.params.get("work_item_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("work_item_id is required")),
    };

    // Verify parent work item exists
    if !stores.work_items.read().unwrap().contains_key(&work_item_id) {
        return DaemonResponse::err(req.id, RpcError::not_found("work_item", &work_item_id));
    }

    let branch_name = req
        .params
        .get("branch_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if branch_name.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("branch_name is required"));
    }

    let base_tick_id = req
        .params
        .get("base_tick_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let claims = req
        .params
        .get("claims")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let bundle = Bundle::new(work_item_id, base_tick_id, branch_name, claims);
    let bundle_json = match serde_json::to_value(&bundle) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(id.clone(), bundle);
    let _ = event_tx.send(DaemonEvent::record_created("bundle", &id));

    DaemonResponse::ok(req.id, bundle_json)
}

fn handle_bundle_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let bundles = stores.bundles.read().unwrap();
    match bundles.get(id) {
        Some(bundle) => match serde_json::to_value(bundle) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("bundle", id)),
    }
}

fn handle_bundle_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let bundles = stores.bundles.read().unwrap();

    // Optionally filter by work_item_id
    let wi_filter = req.params.get("work_item_id").and_then(|v| v.as_str());

    let bundle_list: Vec<&Bundle> = bundles
        .values()
        .filter(|b| wi_filter.is_none() || Some(b.work_item_id.as_str()) == wi_filter)
        .collect();

    match serde_json::to_value(&bundle_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_bundle_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: BundleStatus = match req.params.get("target_status") {
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

    let mut bundles = stores.bundles.write().unwrap();
    let bundle = match bundles.get_mut(&id) {
        Some(b) => b,
        None => return DaemonResponse::err(req.id, RpcError::not_found("bundle", &id)),
    };

    let from = bundle.status;
    let rules = bundle_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    bundle.status = target_status;
    bundle.updated_at = crate::id::now_millis();

    let bundle_json = match serde_json::to_value(&*bundle) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "bundle",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, bundle_json)
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
        let req = DaemonRequest::new(1, "spec.create", json!({"plan_id": "nonexistent", "title": "Spec"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_spec_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let plan_id = create_test_plan(&stores, &tx);
        let req = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "description": "no title"}));
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

        let req = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"}));
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

        let get_resp = dispatch(&stores, &tx, DaemonRequest::new(3, "spec.get", json!({"id": spec_id})));
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

    // --- phase handlers ---

    /// Helper: create a plan + spec and return (plan_id, spec_id)
    fn create_test_spec(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>) -> (String, String) {
        let plan_id = create_test_plan(stores, tx);
        let resp = dispatch(
            stores,
            tx,
            DaemonRequest::new(10, "spec.create", json!({"plan_id": plan_id, "title": "Parent Spec"})),
        );
        let spec_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id)
    }

    #[test]
    fn test_phase_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx);

        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({
                "spec_id": spec_id,
                "title": "Test Phase",
                "description": "A phase",
                "order": 1
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Phase");
        assert_eq!(result["spec_id"], spec_id);
        assert_eq!(result["status"], "draft");
        assert_eq!(result["order"], 1);
        assert_eq!(stores.phases.read().unwrap().len(), 1);
    }

    #[test]
    fn test_phase_create_missing_spec_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "phase.create", json!({"title": "Phase"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("spec_id"));
    }

    #[test]
    fn test_phase_create_spec_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "phase.create", json!({"spec_id": "nonexistent", "title": "Phase"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_phase_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx);
        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"spec_id": spec_id, "description": "no title"}),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_phase_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx);
        // Drain plan+spec create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"}));
        dispatch(&stores, &tx, req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "phase");
    }

    #[test]
    fn test_phase_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"spec_id": spec_id, "title": "My Phase", "order": 3}),
            ),
        );
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(21, "phase.get", json!({"id": phase_id})),
        );
        assert!(!get_resp.is_error());
        let result = get_resp.result.unwrap();
        assert_eq!(result["title"], "My Phase");
        assert_eq!(result["order"], 3);
    }

    #[test]
    fn test_phase_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "phase.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_phase_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "phase.list", json!(null));
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_phase_list_filtered_by_spec_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, spec_id_1) = create_test_spec(&stores, &tx);

        // Create a second spec under the same plan
        let plan_id = _plan_id;
        let resp2 = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(11, "spec.create", json!({"plan_id": plan_id, "title": "Spec 2"})),
        );
        let spec_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create phases under different specs
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"spec_id": spec_id_1, "title": "Phase A", "order": 1}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": spec_id_2, "title": "Phase B", "order": 1}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(&stores, &tx, DaemonRequest::new(30, "phase.list", json!(null)));
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by spec_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(31, "phase.list", json!({"spec_id": spec_id_1})),
        );
        let phases = filtered_resp.result.unwrap();
        let arr = phases.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Phase A");
    }

    #[test]
    fn test_phase_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx);
        // Drain plan+spec create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"})),
        );
        let _ = rx.try_recv(); // consume phase create event
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            21,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "phase");
        assert_eq!(event.data["from"], "Draft");
        assert_eq!(event.data["to"], "Active");
    }

    #[test]
    fn test_phase_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"})),
        );
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            21,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "complete",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_phase_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"})),
        );
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            21,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "active",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_phase_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(
            1,
            "phase.transition",
            json!({
                "id": "nonexistent",
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    // --- work_item handlers ---

    /// Helper: create a plan + spec + phase and return (plan_id, spec_id, phase_id)
    fn create_test_phase(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>) -> (String, String, String) {
        let (plan_id, spec_id) = create_test_spec(stores, tx);
        let resp = dispatch(
            stores,
            tx,
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Parent Phase", "order": 1}),
            ),
        );
        let phase_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id, phase_id)
    }

    #[test]
    fn test_work_item_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx);

        let req = DaemonRequest::new(
            30,
            "work_item.create",
            json!({
                "phase_id": phase_id,
                "title": "Implement auth",
                "description": "Add JWT signing"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Implement auth");
        assert_eq!(result["phase_id"], phase_id);
        assert_eq!(result["status"], "Draft");
        assert_eq!(stores.work_items.read().unwrap().len(), 1);
    }

    #[test]
    fn test_work_item_create_missing_phase_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "work_item.create", json!({"title": "WI"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("phase_id"));
    }

    #[test]
    fn test_work_item_create_phase_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "work_item.create", json!({"phase_id": "nonexistent", "title": "WI"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_item_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx);
        let req = DaemonRequest::new(
            30,
            "work_item.create",
            json!({"phase_id": phase_id, "description": "no title"}),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_work_item_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx);
        // Drain plan+spec+phase create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "WI"}));
        dispatch(&stores, &tx, req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "work_item");
    }

    #[test]
    fn test_work_item_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "My WI"})),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(31, "work_item.get", json!({"id": wi_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My WI");
    }

    #[test]
    fn test_work_item_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "work_item.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_item_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "work_item.list", json!(null));
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_work_item_list_filtered_by_phase_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, spec_id, phase_id_1) = create_test_phase(&stores, &tx);

        // Create a second phase under the same spec
        let resp2 = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Phase 2", "order": 2}),
            ),
        );
        let phase_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create work items under different phases
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id_1, "title": "WI A"})),
        );
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(31, "work_item.create", json!({"phase_id": phase_id_2, "title": "WI B"})),
        );

        // List all — should have 2
        let all_resp = dispatch(&stores, &tx, DaemonRequest::new(40, "work_item.list", json!(null)));
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by phase_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(41, "work_item.list", json!({"phase_id": phase_id_1})),
        );
        let items = filtered_resp.result.unwrap();
        let arr = items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "WI A");
    }

    #[test]
    fn test_work_item_transition_draft_to_ready() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx);
        // Drain plan+spec+phase create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "WI"})),
        );
        let _ = rx.try_recv(); // consume work_item create event
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            31,
            "work_item.transition",
            json!({
                "id": wi_id,
                "target_status": "Ready",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Ready");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "work_item");
        assert_eq!(event.data["from"], "Draft");
        assert_eq!(event.data["to"], "Ready");
    }

    #[test]
    fn test_work_item_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "WI"})),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Draft → InProgress (invalid: must go through Ready)
        let req = DaemonRequest::new(
            31,
            "work_item.transition",
            json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_work_item_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "WI"})),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition Draft → Ready
        let req = DaemonRequest::new(
            31,
            "work_item.transition",
            json!({
                "id": wi_id,
                "target_status": "Ready",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_work_item_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(
            1,
            "work_item.transition",
            json!({
                "id": "nonexistent",
                "target_status": "Ready"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    // --- bundle handlers ---

    /// Helper: create plan + spec + phase + work_item and return (phase_id, work_item_id)
    fn create_test_work_item(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>) -> (String, String) {
        let (_plan_id, _spec_id, phase_id) = create_test_phase(stores, tx);
        let resp = dispatch(
            stores,
            tx,
            DaemonRequest::new(
                30,
                "work_item.create",
                json!({"phase_id": phase_id, "title": "Parent WI"}),
            ),
        );
        let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (phase_id, wi_id)
    }

    #[test]
    fn test_bundle_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": "tick-001",
                "claims": "Add JWT signing"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["work_item_id"], wi_id);
        assert_eq!(result["branch_name"], "feature/auth");
        assert_eq!(result["base_tick_id"], "tick-001");
        assert_eq!(result["claims"], "Add JWT signing");
        assert_eq!(result["status"], "Proposed");
        assert_eq!(stores.bundles.read().unwrap().len(), 1);
    }

    #[test]
    fn test_bundle_create_no_base_tick() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/init"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["base_tick_id"].is_null());
    }

    #[test]
    fn test_bundle_create_missing_work_item_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "bundle.create", json!({"branch_name": "feature/x"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("work_item_id"));
    }

    #[test]
    fn test_bundle_create_work_item_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(
            1,
            "bundle.create",
            json!({"work_item_id": "nonexistent", "branch_name": "feature/x"}),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_bundle_create_missing_branch_name() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx);
        let req = DaemonRequest::new(40, "bundle.create", json!({"work_item_id": wi_id, "claims": "stuff"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("branch_name"));
    }

    #[test]
    fn test_bundle_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx);
        // Drain plan+spec+phase+work_item create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_item_id": wi_id, "branch_name": "feature/x"}),
        );
        dispatch(&stores, &tx, req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "bundle");
    }

    #[test]
    fn test_bundle_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id, "branch_name": "feature/auth"}),
            ),
        );
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(41, "bundle.get", json!({"id": bundle_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["branch_name"], "feature/auth");
    }

    #[test]
    fn test_bundle_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "bundle.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_bundle_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(1, "bundle.list", json!(null));
        let resp = dispatch(&stores, &tx, req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_bundle_list_filtered_by_work_item_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_phase_id, wi_id_1) = create_test_work_item(&stores, &tx);

        // Create a second work item under the same phase
        let resp2 = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(31, "work_item.create", json!({"phase_id": _phase_id, "title": "WI 2"})),
        );
        let wi_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create bundles under different work items
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id_1, "branch_name": "feature/a"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                41,
                "bundle.create",
                json!({"work_item_id": wi_id_2, "branch_name": "feature/b"}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(&stores, &tx, DaemonRequest::new(50, "bundle.list", json!(null)));
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by wi_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(51, "bundle.list", json!({"work_item_id": wi_id_1})),
        );
        let bundles = filtered_resp.result.unwrap();
        let arr = bundles.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["branch_name"], "feature/a");
    }

    #[test]
    fn test_bundle_transition_proposed_to_triaged() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx);
        // Drain plan+spec+phase+work_item create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id, "branch_name": "feature/x"}),
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
        let resp = dispatch(&stores, &tx, req);
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
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id, "branch_name": "feature/x"}),
            ),
        );
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Proposed → Accepted (invalid: must go through Triaged → Reviewed)
        let req = DaemonRequest::new(
            41,
            "bundle.transition",
            json!({
                "id": bundle_id,
                "target_status": "Accepted",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_bundle_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx);

        let create_resp = dispatch(
            &stores,
            &tx,
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id, "branch_name": "feature/x"}),
            ),
        );
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition Proposed → Triaged
        let req = DaemonRequest::new(
            41,
            "bundle.transition",
            json!({
                "id": bundle_id,
                "target_status": "Triaged",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_bundle_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let req = DaemonRequest::new(
            1,
            "bundle.transition",
            json!({
                "id": "nonexistent",
                "target_status": "Triaged"
            }),
        );
        let resp = dispatch(&stores, &tx, req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }
}
