use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::domain::role::Role;
use crate::domain::tick::{Tick, TickStatus, tick_transitions};
use crate::domain::transition::validate_transition;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

pub(super) fn handle_tick_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_create()");
        // Singleton guard: at most one non-terminal Tick at a time
        {
            let ticks = stores.read_ticks()?;
            let active = ticks.values().any(|t| !t.status.is_terminal());
            if active {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("A non-terminal Tick already exists"),
                ));
            }
        }

        let number = match req.params.get("number").and_then(|v| v.as_u64()) {
            Some(n) => n as u32,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("number is required (positive integer)"),
                ));
            }
        };

        let tick = Tick::new(number);
        let tick_json = match serde_json::to_value(&tick) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = tick.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(tick.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_ticks()?.insert(id.clone(), tick);
        let _ = event_tx.send(DaemonEvent::record_created("tick", &id));

        Ok(DaemonResponse::ok(req.id, tick_json))
    })
}

pub(super) fn handle_tick_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Tick>(id)
            {
                Ok(Some(tick)) => {
                    return match serde_json::to_value(&tick) {
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

        let ticks = stores.read_ticks()?;
        match ticks.get(id) {
            Some(tick) => match serde_json::to_value(tick) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", id))),
        }
    })
}

pub(super) fn handle_tick_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_list()");
        // Optionally filter by status
        let status_filter: Option<TickStatus> = req
            .params
            .get("status")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let filters: Vec<Filter> = if let Some(status) = &status_filter {
                vec![Filter {
                    field: "status".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(status.to_string()),
                }]
            } else {
                vec![]
            };
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Tick>(&filters)
            {
                Ok(ticks) => {
                    return match serde_json::to_value(&ticks) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let ticks = stores.read_ticks()?;
        let tick_list: Vec<&Tick> = ticks
            .values()
            .filter(|t| status_filter.is_none() || Some(t.status) == status_filter)
            .collect();

        match serde_json::to_value(&tick_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_tick_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: TickStatus = match req.params.get("target_status") {
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
            None => Role::Integrator,
        };

        let mut ticks = stores.write_ticks()?;

        // Read current status immutably first for validation
        let from = match ticks.get(&id) {
            Some(t) => t.status,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", &id))),
        };

        let rules = tick_transitions();
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
            let _ = event_tx.send(DaemonEvent::transition_rejected(
                "ticks",
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

        // Gap #16: Only one Tick in Sealing/Validating at a time
        if matches!(target_status, TickStatus::Sealing | TickStatus::Validating) {
            let has_active = ticks
                .values()
                .any(|t| t.id != id && matches!(t.status, TickStatus::Sealing | TickStatus::Validating));
            if has_active {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("Another Tick is already in Sealing/Validating"),
                ));
            }
        }

        // Now get mutable reference and apply the transition
        let tick = ticks.get_mut(&id).ok_or_else(|| eyre!("record not found: {id}"))?;
        tick.status = target_status;
        tick.updated_at = crate::id::now_millis();
        let tick_clone = tick.clone();
        drop(ticks);

        // Persist transition to TaskStore if available (matches work_transition pattern)
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(tick_clone.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let tick_json = match serde_json::to_value(&tick_clone) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        debug!(
            "[transition] tick.{}: {:?} -> {:?} by {}",
            id, from, target_status, role
        );
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "tick",
            &id,
            &from.to_string(),
            &target_status.to_string(),
            &role.to_string(),
        ));

        Ok(DaemonResponse::ok(req.id, tick_json))
    })
}

pub(super) fn handle_tick_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_update()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut ticks = stores.write_ticks()?;
        let tick = match ticks.get_mut(&id) {
            Some(t) => t,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("ticks", &id))),
        };

        if let Some(log) = req.params.get("validation_log").and_then(|v| v.as_str()) {
            tick.validation_log = log.to_string();
        }
        if let Some(bids) = req.params.get("bundle_ids").and_then(|v| v.as_array()) {
            tick.bundle_ids = bids.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(abids) = req.params.get("attempted_bundle_ids").and_then(|v| v.as_array()) {
            tick.attempted_bundle_ids = abids.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        tick.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(tick.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let tick_json = serde_json::to_value(&*tick)?;
        let _ = event_tx.send(DaemonEvent::record_updated("ticks", &id));
        Ok(DaemonResponse::ok(req.id, tick_json))
    })
}
