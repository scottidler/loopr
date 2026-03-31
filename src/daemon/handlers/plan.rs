use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::domain::plan::{HierarchyStatus, Plan, PlanStatus, hierarchy_transitions};
use crate::domain::role::Role;
use crate::domain::transition::validate_transition;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use crate::daemon::context::Stores;

use super::common::check_validation_gate;

pub(super) fn handle_plan_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_create()");
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
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("title is required"),
            ));
        }

        // Reject if a Draft Plan already exists — Coordinator must abandon the old one first
        {
            let plans = stores.read_plans()?;
            if plans.values().any(|p| p.status == HierarchyStatus::Draft) {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("A Draft Plan already exists; abandon it before creating a new one"),
                ));
            }
        }

        let plan = Plan::new(title, description, acceptance_criteria);
        let plan_json = match serde_json::to_value(&plan) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = plan.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(plan.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_plans()?.insert(id.clone(), plan);
        let _ = event_tx.send(DaemonEvent::record_created("plan", &id));

        Ok(DaemonResponse::ok(req.id, plan_json))
    })
}

pub(super) fn handle_plan_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Plan>(id)
            {
                Ok(Some(plan)) => {
                    return match serde_json::to_value(&plan) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Ok(None) => {}
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let plans = stores.read_plans()?;
        match plans.get(id) {
            Some(plan) => match serde_json::to_value(plan) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", id))),
        }
    })
}

pub(super) fn handle_plan_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_list()");
        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Plan>(&[])
            {
                Ok(plans) => {
                    return match serde_json::to_value(&plans) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let plans = stores.read_plans()?;
        let plan_list: Vec<&Plan> = plans.values().collect();
        match serde_json::to_value(&plan_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_plan_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: PlanStatus = match req.params.get("target_status") {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(s) => s,
                Err(_) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::invalid_params("invalid target_status"),
                    ));
                }
            },
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("target_status is required"),
                ));
            }
        };

        let role: Role = match req.params.get("role") {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(r) => r,
                Err(_) => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("invalid role"))),
            },
            None => Role::Coordinator,
        };

        let skip_validation = req
            .params
            .get("skip_validation")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut plans = stores.write_plans()?;
        let plan = match plans.get_mut(&id) {
            Some(p) => p,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &id))),
        };

        let from = plan.status;
        let rules = hierarchy_transitions();
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
            let _ = event_tx.send(DaemonEvent::transition_rejected(
                "plans",
                &id,
                &format!("{:?}", from),
                &format!("{:?}", target_status),
                &role.to_string(),
                &e.to_string(),
            ));
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::transition_rejected(&e.to_string()),
            ));
        }

        // Validation gate: Draft → Active requires passing validation report
        let skip_reason = req.params.get("skip_reason").and_then(|v| v.as_str());
        if let Some(err) = check_validation_gate(
            stores,
            event_tx,
            from,
            target_status,
            "plan",
            &id,
            skip_validation,
            skip_reason,
        ) {
            return Ok(DaemonResponse::err(req.id, err));
        }

        plan.status = target_status;
        plan.updated_at = crate::id::now_millis();
        let plan_clone = plan.clone();
        drop(plans);

        // Persist transition to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(plan_clone.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let plan_json = match serde_json::to_value(&plan_clone) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        debug!(
            "[transition] plan.{}: {:?} -> {:?} by {}",
            id, from, target_status, role
        );
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "plan",
            &id,
            &from.to_string(),
            &target_status.to_string(),
            &role.to_string(),
        ));

        Ok(DaemonResponse::ok(req.id, plan_json))
    })
}

pub(super) fn handle_plan_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_update()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut plans = stores.write_plans()?;
        let plan = match plans.get_mut(&id) {
            Some(p) => p,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("plans", &id))),
        };

        if let Some(title) = req.params.get("title").and_then(|v| v.as_str()) {
            plan.title = title.to_string();
        }
        if let Some(desc) = req.params.get("description").and_then(|v| v.as_str()) {
            plan.description = desc.to_string();
        }
        if let Some(criteria) = req.params.get("acceptance_criteria").and_then(|v| v.as_str()) {
            plan.acceptance_criteria = criteria.to_string();
        }
        plan.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(plan.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let plan_json = serde_json::to_value(&*plan)?;
        let _ = event_tx.send(DaemonEvent::record_updated("plans", &id));
        Ok(DaemonResponse::ok(req.id, plan_json))
    })
}
