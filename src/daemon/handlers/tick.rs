use std::sync::Arc;

use eyre::eyre;
use tokio::sync::broadcast;
use tracing::{debug, instrument};

use crate::domain::role::Role;
use crate::domain::tick::{Tick, TickStatus};
use crate::domain::transition::Transition;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

use super::{parse_optional_param, parse_required_param};

#[instrument(skip_all, fields(number = ?req.params.get("number")))]
pub(super) fn handle_tick_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        // Singleton guard: at most one non-terminal Tick at a time
        {
            let ticks = stores.read_ticks()?;
            let active = ticks.values().any(|t| !t.status().is_terminal());
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

#[instrument(skip_all, fields(id = ?req.params.get("id")))]
pub(super) fn handle_tick_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
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

#[instrument(skip_all)]
pub(super) fn handle_tick_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
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
            .filter(|t| status_filter.is_none() || Some(t.status()) == status_filter)
            .collect();

        match serde_json::to_value(&tick_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

#[instrument(skip_all, fields(id = ?req.params.get("id"), target_status = ?req.params.get("target_status")))]
pub(super) fn handle_tick_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: TickStatus = match parse_required_param(&req, "target_status") {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        };

        let role: Role = match parse_optional_param(&req, "role", Role::Integrator) {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        };

        let mut ticks = stores.write_ticks()?;

        // Read current status immutably first for validation
        let from = match ticks.get(&id) {
            Some(t) => t.status(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", &id))),
        };

        match from.validate_transition(target_status, role) {
            Err(e) => {
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
            Ok(Transition::Unchanged) => {
                return Ok(DaemonResponse::ok(req.id, serde_json::Value::Null));
            }
            Ok(Transition::Changed) => {}
        }

        // Gap #16: Only one Tick in Sealing/Validating at a time
        if matches!(target_status, TickStatus::Sealing | TickStatus::Validating) {
            let has_active = ticks
                .values()
                .any(|t| t.id != id && matches!(t.status(), TickStatus::Sealing | TickStatus::Validating));
            if has_active {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("Another Tick is already in Sealing/Validating"),
                ));
            }
        }

        // Now get mutable reference and apply the transition
        let tick = ticks.get_mut(&id).ok_or_else(|| eyre!("record not found: {id}"))?;
        tick.force_status(target_status);
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

#[instrument(skip_all, fields(id = ?req.params.get("id")))]
pub(super) fn handle_tick_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
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
    use crate::domain::tick::Tick;
    use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
    use crate::worktree::manager::WorktreeManager;

    /// Helper: create a tick and return its id
    async fn create_test_tick(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        )
        .await;
        assert!(!resp.is_error(), "tick.create failed: {:?}", resp.error);
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    // === Tests from mod.rs lines 3519-4074 ===

    #[tokio::test]
    async fn test_tick_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(50, "tick.create", json!({"number": 7}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Tick> = store.get(&tick_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().number, 7);
    }

    #[tokio::test]
    async fn test_tick_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(50, "tick.create", json!({"number": 1}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["number"], 1);
        assert_eq!(result["status"], "Open");
        assert!(result["integration_sha"].is_null());
        assert_eq!(result["bundle_ids"].as_array().unwrap().len(), 0);
        assert_eq!(stores.ticks.read().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_tick_create_missing_number() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tick.create", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("number"));
    }

    #[tokio::test]
    async fn test_tick_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();

        let req = DaemonRequest::new(50, "tick.create", json!({"number": 1}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "tick");
    }

    #[tokio::test]
    async fn test_tick_create_singleton_guard_blocks_second() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create first tick (Open)
        let resp1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        )
        .await;
        assert!(!resp1.is_error());

        // Second create should fail - non-terminal Tick exists
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(51, "tick.create", json!({"number": 2})),
        )
        .await;
        assert!(resp2.is_error());
        assert!(
            resp2
                .error
                .unwrap()
                .message
                .contains("non-terminal Tick already exists")
        );
    }

    #[tokio::test]
    async fn test_tick_create_singleton_guard_allows_after_terminal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create and publish first tick
        let c1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        )
        .await;
        let t1_id = c1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                51,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Validating", "role": "integrator"}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                53,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Published", "role": "integrator"}),
            ),
        )
        .await;

        // Now creation should succeed
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(54, "tick.create", json!({"number": 2})),
        )
        .await;
        assert!(!resp2.is_error());
    }

    #[tokio::test]
    async fn test_tick_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 42})),
        )
        .await;
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "tick.get", json!({"id": tick_id})),
        )
        .await;
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["number"], 42);
    }

    #[tokio::test]
    async fn test_tick_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tick.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_tick_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a tick (writes to both TaskStore and HashMap)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 99})),
        )
        .await;
        assert!(!create_resp.is_error());
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.ticks.write().unwrap().remove(&tick_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(51, "tick.get", json!({"id": tick_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req).await;
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["number"], 99);
    }

    #[tokio::test]
    async fn test_tick_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tick.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_tick_list_filtered_by_status() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create first tick
        let create1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        )
        .await;
        let tick1_id = create1.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition tick 1 through to Published so we can create another
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": tick1_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                53,
                "tick.transition",
                json!({"id": tick1_id, "target_status": "Validating", "role": "integrator"}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                54,
                "tick.transition",
                json!({"id": tick1_id, "target_status": "Published", "role": "integrator"}),
            ),
        )
        .await;

        // Now create second tick (singleton guard allows it since tick 1 is terminal)
        let create2 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(55, "tick.create", json!({"number": 2})),
        )
        .await;
        let tick2_id = create2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition tick 2 to Sealing
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                56,
                "tick.transition",
                json!({"id": tick2_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        )
        .await;

        // List all - should have 2
        let all_resp = dispatch(&stores, &tx, &wm, &ic, DaemonRequest::new(60, "tick.list", json!(null))).await;
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by Published - should have 1 (tick 1)
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(61, "tick.list", json!({"status": "Published"})),
        )
        .await;
        let ticks = filtered_resp.result.unwrap();
        let arr = ticks.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["number"], 1);
    }

    #[tokio::test]
    async fn test_tick_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create first tick, transition to Published, then create second
        let c1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        )
        .await;
        let t1_id = c1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                53,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Validating", "role": "integrator"}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                54,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Published", "role": "integrator"}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(55, "tick.create", json!({"number": 2})),
        )
        .await;

        // Clear HashMap to prove list reads from TaskStore
        stores.ticks.write().unwrap().clear();

        // List all should still return both ticks via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(60, "tick.list", json!(null)),
        )
        .await;
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list by status also works from TaskStore
        // Tick 1 is Published, tick 2 is Open
        let filtered_req = DaemonRequest::new(61, "tick.list", json!({"status": "Open"}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req).await;
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        let arr = filtered_items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[tokio::test]
    async fn test_tick_transition_open_to_sealing() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        )
        .await;
        let _ = rx.try_recv(); // consume create event
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing", "role": "integrator"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Sealing");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "tick");
        assert_eq!(event.data["from"], "Open");
        assert_eq!(event.data["to"], "Sealing");
    }

    #[tokio::test]
    async fn test_tick_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        )
        .await;
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Open -> Published (invalid: must go through Sealing -> Validating)
        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Published", "role": "integrator"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[tokio::test]
    async fn test_tick_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        )
        .await;
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Coordinator cannot transition tick (Integrator-only)
        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing", "role": "coordinator"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[tokio::test]
    async fn test_tick_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tick.transition",
            json!({"id": "nonexistent", "target_status": "Sealing"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_tick_transition_default_role_is_integrator() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        )
        .await;
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Omit role - should default to Integrator and succeed
        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Sealing");
    }

    // === Tests from mod.rs lines 7781-7839 ===

    #[tokio::test]
    async fn test_handle_tick_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_test_tick(&stores, &tx, &wm).await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "tick.update",
                json!({
                    "id": tick_id,
                    "validation_log": "All tests passed",
                    "bundle_ids": ["b-1", "b-2"],
                    "attempted_bundle_ids": ["b-1", "b-2", "b-3"]
                }),
            ),
        )
        .await;
        assert!(!resp.is_error(), "tick.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["validation_log"], "All tests passed");
        assert_eq!(result["bundle_ids"].as_array().unwrap().len(), 2);
        assert_eq!(result["attempted_bundle_ids"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_handle_tick_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.update", json!({"id": "nonexistent", "validation_log": "x"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_tick_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.update", json!({"validation_log": "x"})),
        )
        .await;
        assert!(resp.is_error());
    }
}
