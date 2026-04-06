use std::sync::Arc;

use eyre::eyre;
use tokio::sync::broadcast;
use tracing::debug;

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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::broadcast;

    use crate::daemon::context::Stores;
    use crate::daemon::handlers::dispatch;
    use crate::daemon::handlers::tests::{
        test_event_tx, test_integrator_config, test_stores, test_stores_with_taskstore, test_worktree_mgr,
    };
    use crate::domain::lock::Lock;
    use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
    use crate::worktree::manager::WorktreeManager;

    async fn create_lock(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
        id: u64,
    ) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                id,
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord-1"}),
            ),
        )
        .await;
        assert!(!resp.is_error());
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn test_lock_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "lock.create",
            json!({
                "resource": "src/main.rs",
                "holder_id": "wi-1",
                "granted_by": "coord-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let lock_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Lock> = store.get(&lock_id).unwrap();
        assert!(retrieved.is_some());
        let lock = retrieved.unwrap();
        assert_eq!(lock.resource, "src/main.rs");
        assert_eq!(lock.holder_id, "wi-1");
        assert_eq!(lock.granted_by, "coord-1");
    }

    #[tokio::test]
    async fn test_lock_create() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord-1"}),
            ),
        )
        .await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["resource"], "src/main.rs");
        assert_eq!(result["holder_id"], "wi-1");
        assert_eq!(result["granted_by"], "coord-1");
        assert_eq!(result["status"], "active");
    }

    #[tokio::test]
    async fn test_lock_create_missing_resource() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "lock.create", json!({"holder_id": "wi-1", "granted_by": "coord-1"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_lock_create_missing_holder_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "file.rs", "granted_by": "coord-1"}),
            ),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_lock_get() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1).await;
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.get", json!({"id": lock_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["resource"], "src/main.rs");
    }

    #[tokio::test]
    async fn test_lock_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "lock.get", json!({"id": "nonexistent"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_lock_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a lock (writes to both TaskStore and HashMap)
        let lock_id = create_lock(&stores, &tx, &wm, 50).await;

        // Remove from HashMap to prove get reads from TaskStore
        stores.locks.write().unwrap().remove(&lock_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(51, "lock.get", json!({"id": lock_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req).await;
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["resource"], "src/main.rs");
    }

    #[tokio::test]
    async fn test_lock_list() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        create_lock(&stores, &tx, &wm, 1).await;
        create_lock(&stores, &tx, &wm, 2).await;
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.list", json!({})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_lock_list_filter_active_only() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1).await;
        create_lock(&stores, &tx, &wm, 2).await;

        // Release the first lock
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.release", json!({"id": lock_id})),
        )
        .await;

        // List active only
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "lock.list", json!({"active_only": true})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_lock_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create two locks (writes to both TaskStore and HashMap)
        create_lock(&stores, &tx, &wm, 1).await;
        // Create a second lock with different resource
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "lock.create",
                json!({"resource": "src/lib.rs", "holder_id": "wi-2", "granted_by": "coord-1"}),
            ),
        )
        .await;

        // Clear HashMap to prove list reads from TaskStore
        stores.locks.write().unwrap().clear();

        // List all should still return both locks via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "lock.list", json!(null)),
        )
        .await;
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test active_only filter works from TaskStore (both are Active)
        let active_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "lock.list", json!({"active_only": true})),
        )
        .await;
        assert!(!active_resp.is_error());
        assert_eq!(active_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test resource filter works from TaskStore
        let resource_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(12, "lock.list", json!({"resource": "src/lib.rs"})),
        )
        .await;
        assert!(!resource_resp.is_error());
        let resource_items = resource_resp.result.unwrap();
        assert_eq!(resource_items.as_array().unwrap().len(), 1);
        assert_eq!(resource_items[0]["resource"], "src/lib.rs");
    }

    #[tokio::test]
    async fn test_lock_release() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1).await;
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.release", json!({"id": lock_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "released");
    }

    #[tokio::test]
    async fn test_lock_release_already_released() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1).await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.release", json!({"id": lock_id})),
        )
        .await;
        // Try releasing again
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.release", json!({"id": lock_id})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_lock_expire() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1).await;
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.expire", json!({"id": lock_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "expired");
    }

    #[tokio::test]
    async fn test_lock_expire_already_expired() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1).await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.expire", json!({"id": lock_id})),
        )
        .await;
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.expire", json!({"id": lock_id})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_lock_release_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "lock.create",
            json!({
                "resource": "src/main.rs",
                "holder_id": "wi-1",
                "granted_by": "coord-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let lock_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "lock.release", json!({"id": lock_id})),
        )
        .await;
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let lock: Option<Lock> = store.get(&lock_id).unwrap();
        assert!(lock.is_some());
        assert_eq!(lock.unwrap().status().to_string(), "Released");
    }

    #[tokio::test]
    async fn test_lock_expire_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "lock.create",
            json!({
                "resource": "src/main.rs",
                "holder_id": "wi-1",
                "granted_by": "coord-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let lock_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "lock.expire", json!({"id": lock_id})),
        )
        .await;
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let lock: Option<Lock> = store.get(&lock_id).unwrap();
        assert!(lock.is_some());
        assert_eq!(lock.unwrap().status().to_string(), "Expired");
    }

    #[tokio::test]
    async fn test_lock_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        create_lock(&stores, &tx, &wm, 1).await;
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "lock");
    }

    #[tokio::test]
    async fn test_lock_release_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let lock_id = create_lock(&stores, &tx, &wm, 1).await;
        let _ = rx.try_recv(); // consume create event

        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.release", json!({"id": lock_id})),
        )
        .await;
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.updated");
        assert_eq!(event.data["collection"], "lock");
    }

    #[tokio::test]
    async fn test_lock_create_with_ttl_param() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord-1", "ttl_secs": 300}),
            ),
        )
        .await;
        assert!(!resp.is_error(), "lock.create with ttl failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert!(result["expires_at"].is_number(), "should have expires_at from ttl_secs");
    }

    #[tokio::test]
    async fn test_lock_create_auto_expire() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Default max_lock_ttl_minutes is 60, so auto-expire should set expires_at
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "src/lib.rs", "holder_id": "wi-2", "granted_by": "coord-1"}),
            ),
        )
        .await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        // Without explicit ttl_secs, auto-expire from max_lock_ttl_minutes should set expires_at
        assert!(result["expires_at"].is_number(), "should have auto-expire expires_at");
    }

    #[tokio::test]
    async fn test_lock_create_renewable_flag() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "src/mod.rs", "holder_id": "wi-3", "granted_by": "coord-1", "renewable": true}),
            ),
        )
        .await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["renewable"], true);
    }
}
