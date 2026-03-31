use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::domain::plan::{HierarchyStatus, hierarchy_transitions};
use crate::domain::role::Role;
use crate::domain::spec::{Spec, SpecStatus};
use crate::domain::transition::validate_transition;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

use super::common::check_validation_gate;

pub(super) fn handle_spec_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_create()");
        let plan_id = match req.params.get("plan_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("plan_id is required"),
                ));
            }
        };

        // Verify parent plan exists and is not in a terminal state
        {
            let plans = stores.read_plans()?;
            match plans.get(&plan_id) {
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &plan_id))),
                Some(plan) if matches!(plan.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Cannot create spec under {} plan '{}'",
                            plan.status, plan_id
                        )),
                    ));
                }
                _ => {}
            }
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
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("title is required"),
            ));
        }

        // Reject if a Draft Spec already exists under this Plan
        {
            let specs = stores.read_specs()?;
            if specs
                .values()
                .any(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Draft)
            {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(
                        "A Draft Spec already exists under this Plan; abandon it before creating a new one",
                    ),
                ));
            }
        }

        let spec = Spec::new(plan_id, title, description);
        let spec_json = match serde_json::to_value(&spec) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = spec.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(spec.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_specs()?.insert(id.clone(), spec);
        let _ = event_tx.send(DaemonEvent::record_created("spec", &id));

        Ok(DaemonResponse::ok(req.id, spec_json))
    })
}

pub(super) fn handle_spec_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Spec>(id)
            {
                Ok(Some(spec)) => {
                    return match serde_json::to_value(&spec) {
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

        let specs = stores.read_specs()?;
        match specs.get(id) {
            Some(spec) => match serde_json::to_value(spec) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", id))),
        }
    })
}

pub(super) fn handle_spec_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_list()");
        let plan_id_filter = req.params.get("plan_id").and_then(|v| v.as_str());

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let filters: Vec<Filter> = if let Some(pid) = plan_id_filter {
                vec![Filter {
                    field: "plan_id".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(pid.to_string()),
                }]
            } else {
                vec![]
            };
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Spec>(&filters)
            {
                Ok(specs) => {
                    return match serde_json::to_value(&specs) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let specs = stores.read_specs()?;
        let spec_list: Vec<&Spec> = specs
            .values()
            .filter(|s| plan_id_filter.is_none() || Some(s.plan_id.as_str()) == plan_id_filter)
            .collect();

        match serde_json::to_value(&spec_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_spec_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: SpecStatus = match req.params.get("target_status") {
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

        let mut specs = stores.write_specs()?;
        let spec = match specs.get_mut(&id) {
            Some(s) => s,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &id))),
        };

        let from = spec.status;
        let rules = hierarchy_transitions();
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
            let _ = event_tx.send(DaemonEvent::transition_rejected(
                "specs",
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
            "spec",
            &id,
            skip_validation,
            skip_reason,
        ) {
            return Ok(DaemonResponse::err(req.id, err));
        }

        spec.status = target_status;
        spec.updated_at = crate::id::now_millis();
        let spec_clone = spec.clone();
        drop(specs);

        // Persist transition to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(spec_clone.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let spec_json = match serde_json::to_value(&spec_clone) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        debug!(
            "[transition] spec.{}: {:?} -> {:?} by {}",
            id, from, target_status, role
        );
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "spec",
            &id,
            &from.to_string(),
            &target_status.to_string(),
            &role.to_string(),
        ));

        Ok(DaemonResponse::ok(req.id, spec_json))
    })
}

pub(super) fn handle_spec_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_update()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut specs = stores.write_specs()?;
        let spec = match specs.get_mut(&id) {
            Some(s) => s,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("specs", &id))),
        };

        if let Some(title) = req.params.get("title").and_then(|v| v.as_str()) {
            spec.title = title.to_string();
        }
        if let Some(desc) = req.params.get("description").and_then(|v| v.as_str()) {
            spec.description = desc.to_string();
        }
        spec.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(spec.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let spec_json = serde_json::to_value(&*spec)?;
        let _ = event_tx.send(DaemonEvent::record_updated("specs", &id));
        Ok(DaemonResponse::ok(req.id, spec_json))
    })
}
