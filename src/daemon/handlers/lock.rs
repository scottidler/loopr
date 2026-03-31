use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::domain::lock::Lock;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

pub(super) fn handle_lock_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_create()");
        let resource = req
            .params
            .get("resource")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let holder_id = req
            .params
            .get("holder_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let granted_by = req
            .params
            .get("granted_by")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if resource.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("resource is required"),
            ));
        }
        if holder_id.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("holder_id is required"),
            ));
        }
        if granted_by.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("granted_by is required"),
            ));
        }

        let mut lock = Lock::new(resource, holder_id, granted_by);

        // #11: Accept optional ttl_secs param; compute expires_at
        if let Some(ttl_secs) = req.params.get("ttl_secs").and_then(|v| v.as_u64()) {
            lock.expires_at = Some(crate::id::now_millis() + (ttl_secs as i64 * 1000));
        }

        // Gap #25: If no explicit TTL, apply max_lock_ttl_minutes from config
        if lock.expires_at.is_none() {
            let ttl_minutes = stores.config.strategy.max_lock_ttl_minutes;
            if ttl_minutes > 0 {
                lock.expires_at = Some(crate::id::now_millis() + (ttl_minutes as i64 * 60 * 1000));
            }
        }
        if let Some(renewable) = req.params.get("renewable").and_then(|v| v.as_bool()) {
            lock.renewable = renewable;
        }

        // Auto-expire any locks that have passed their TTL
        {
            let mut locks = stores.write_locks()?;
            for existing_lock in locks.values_mut() {
                if existing_lock.is_active() && existing_lock.is_expired() {
                    existing_lock.expire();
                }
            }
        }

        let lock_json = match serde_json::to_value(&lock) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = lock.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(lock.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_locks()?.insert(id.clone(), lock);
        let _ = event_tx.send(DaemonEvent::record_created("lock", &id));

        Ok(DaemonResponse::ok(req.id, lock_json))
    })
}

pub(super) fn handle_lock_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Lock>(id)
            {
                Ok(Some(lock)) => {
                    return match serde_json::to_value(&lock) {
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

        let locks = stores.read_locks()?;
        match locks.get(id) {
            Some(lock) => match serde_json::to_value(lock) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("lock", id))),
        }
    })
}

pub(super) fn handle_lock_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_list()");
        // Optionally filter by resource
        let resource_filter = req
            .params
            .get("resource")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Optionally filter by holder_id
        let holder_filter = req
            .params
            .get("holder_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Optionally filter by active-only
        let active_only = req.params.get("active_only").and_then(|v| v.as_bool()).unwrap_or(false);

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let mut filters: Vec<Filter> = vec![];
            if let Some(resource) = &resource_filter {
                filters.push(Filter {
                    field: "resource".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(resource.clone()),
                });
            }
            if let Some(holder_id) = &holder_filter {
                filters.push(Filter {
                    field: "holder_id".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(holder_id.clone()),
                });
            }
            if active_only {
                filters.push(Filter {
                    field: "status".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String("Active".to_string()),
                });
            }
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Lock>(&filters)
            {
                Ok(locks) => {
                    return match serde_json::to_value(&locks) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let locks = stores.read_locks()?;
        let lock_list: Vec<&Lock> = locks
            .values()
            .filter(|l| resource_filter.is_none() || Some(l.resource.as_str()) == resource_filter.as_deref())
            .filter(|l| holder_filter.is_none() || Some(l.holder_id.as_str()) == holder_filter.as_deref())
            .filter(|l| !active_only || l.is_active())
            .collect();

        match serde_json::to_value(&lock_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_lock_release(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_release()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut locks = stores.write_locks()?;
        let lock = match locks.get_mut(&id) {
            Some(l) => l,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("lock", &id))),
        };

        if !lock.is_active() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("lock is not active"),
            ));
        }

        lock.release();
        lock.updated_at = crate::id::now_millis();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(lock.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let lock_json = match serde_json::to_value(&*lock) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("lock", &id));

        Ok(DaemonResponse::ok(req.id, lock_json))
    })
}

pub(super) fn handle_lock_expire(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_expire()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut locks = stores.write_locks()?;
        let lock = match locks.get_mut(&id) {
            Some(l) => l,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("lock", &id))),
        };

        if !lock.is_active() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("lock is not active"),
            ));
        }

        lock.expire();
        lock.updated_at = crate::id::now_millis();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(lock.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let lock_json = match serde_json::to_value(&*lock) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("lock", &id));

        Ok(DaemonResponse::ok(req.id, lock_json))
    })
}
