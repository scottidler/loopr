use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::domain::phase::{Phase, PhaseStatus};
use crate::domain::plan::{HierarchyStatus, hierarchy_transitions};
use crate::domain::role::Role;
use crate::domain::transition::validate_transition;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

use super::common::check_validation_gate;

pub(super) fn handle_phase_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_create()");
        let spec_id = match req.params.get("spec_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("spec_id is required"),
                ));
            }
        };

        // Verify parent spec exists and is not in a terminal state
        {
            let specs = stores.read_specs()?;
            match specs.get(&spec_id) {
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &spec_id))),
                Some(spec) if matches!(spec.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Cannot create phase under {} spec '{}'",
                            spec.status, spec_id
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
        let order = req.params.get("order").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

        if title.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("title is required"),
            ));
        }

        // Reject if a Draft Phase already exists under this Spec
        {
            let phases = stores.read_phases()?;
            if phases
                .values()
                .any(|p| p.spec_id == spec_id && p.status == HierarchyStatus::Draft)
            {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(
                        "A Draft Phase already exists under this Spec; abandon it before creating a new one",
                    ),
                ));
            }
        }

        let phase = Phase::new(spec_id, title, description, order);
        let phase_json = match serde_json::to_value(&phase) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = phase.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(phase.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_phases()?.insert(id.clone(), phase);
        let _ = event_tx.send(DaemonEvent::record_created("phase", &id));

        Ok(DaemonResponse::ok(req.id, phase_json))
    })
}

pub(super) fn handle_phase_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Phase>(id)
            {
                Ok(Some(phase)) => {
                    return match serde_json::to_value(&phase) {
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

        let phases = stores.read_phases()?;
        match phases.get(id) {
            Some(phase) => match serde_json::to_value(phase) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", id))),
        }
    })
}

pub(super) fn handle_phase_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_list()");
        let spec_id_filter = req.params.get("spec_id").and_then(|v| v.as_str());

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let filters: Vec<Filter> = if let Some(sid) = spec_id_filter {
                vec![Filter {
                    field: "spec_id".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(sid.to_string()),
                }]
            } else {
                vec![]
            };
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Phase>(&filters)
            {
                Ok(phases) => {
                    return match serde_json::to_value(&phases) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let phases = stores.read_phases()?;
        let phase_list: Vec<&Phase> = phases
            .values()
            .filter(|p| spec_id_filter.is_none() || Some(p.spec_id.as_str()) == spec_id_filter)
            .collect();

        match serde_json::to_value(&phase_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_phase_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: PhaseStatus = match req.params.get("target_status") {
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

        let mut phases = stores.write_phases()?;
        let phase = match phases.get_mut(&id) {
            Some(p) => p,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", &id))),
        };

        let from = phase.status;
        let rules = hierarchy_transitions();
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
            let _ = event_tx.send(DaemonEvent::transition_rejected(
                "phases",
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
            "phase",
            &id,
            skip_validation,
            skip_reason,
        ) {
            return Ok(DaemonResponse::err(req.id, err));
        }

        phase.status = target_status;
        phase.updated_at = crate::id::now_millis();
        let phase_clone = phase.clone();
        drop(phases);

        // Persist transition to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(phase_clone.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let phase_json = match serde_json::to_value(&phase_clone) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        debug!(
            "[transition] phase.{}: {:?} -> {:?} by {}",
            id, from, target_status, role
        );
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "phase",
            &id,
            &from.to_string(),
            &target_status.to_string(),
            &role.to_string(),
        ));

        Ok(DaemonResponse::ok(req.id, phase_json))
    })
}

pub(super) fn handle_phase_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_update()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut phases = stores.write_phases()?;
        let phase = match phases.get_mut(&id) {
            Some(p) => p,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("phases", &id))),
        };

        if let Some(title) = req.params.get("title").and_then(|v| v.as_str()) {
            phase.title = title.to_string();
        }
        if let Some(desc) = req.params.get("description").and_then(|v| v.as_str()) {
            phase.description = desc.to_string();
        }
        if let Some(order) = req.params.get("order").and_then(|v| v.as_u64()) {
            phase.order = order as u32;
        }
        phase.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(phase.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let phase_json = serde_json::to_value(&*phase)?;
        let _ = event_tx.send(DaemonEvent::record_updated("phases", &id));
        Ok(DaemonResponse::ok(req.id, phase_json))
    })
}
