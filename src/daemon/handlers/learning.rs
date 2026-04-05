use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::domain::learning::{Learning, LearningScope};
use crate::domain::role::Role;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

pub(super) fn handle_learning_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_learning_create()");
        let source_id = req
            .params
            .get("source_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let scope: LearningScope = match req.params.get("scope") {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(s) => s,
                Err(_) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::invalid_params("invalid scope (work|phase|spec|plan|global)"),
                    ));
                }
            },
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("scope is required"),
                ));
            }
        };
        let content = req
            .params
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if source_id.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("source_id is required"),
            ));
        }
        if content.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("content is required"),
            ));
        }

        let mut learning = Learning::new(source_id, scope, content);

        // M3: Parse applicable_roles
        if let Some(roles_val) = req.params.get("applicable_roles")
            && let Ok(roles) = serde_json::from_value::<Vec<Role>>(roles_val.clone())
        {
            learning.applicable_roles = Some(roles);
        }

        // M4: Parse resource_tags
        if let Some(tags_val) = req.params.get("resource_tags")
            && let Ok(tags) = serde_json::from_value::<Vec<String>>(tags_val.clone())
        {
            learning.resource_tags = tags;
        }

        let learning_json = match serde_json::to_value(&learning) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = learning.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(learning.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_learnings()?.insert(id.clone(), learning);
        let _ = event_tx.send(DaemonEvent::record_created("learning", &id));

        Ok(DaemonResponse::ok(req.id, learning_json))
    })
}

pub(super) fn handle_learning_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_learning_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Learning>(id)
            {
                Ok(Some(learning)) => {
                    return match serde_json::to_value(&learning) {
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

        let learnings = stores.read_learnings()?;
        match learnings.get(id) {
            Some(learning) => match serde_json::to_value(learning) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("learning", id))),
        }
    })
}

pub(super) fn handle_learning_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_learning_list()");
        // Optionally filter by scope
        let scope_filter: Option<LearningScope> = req
            .params
            .get("scope")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        // Optionally filter by source_id
        let source_id_filter = req
            .params
            .get("source_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let mut filters: Vec<Filter> = vec![];
            if let Some(scope) = &scope_filter {
                filters.push(Filter {
                    field: "scope".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(scope.to_string()),
                });
            }
            if let Some(source_id) = &source_id_filter {
                filters.push(Filter {
                    field: "source_id".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(source_id.clone()),
                });
            }
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Learning>(&filters)
            {
                Ok(learnings) => {
                    return match serde_json::to_value(&learnings) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let learnings = stores.read_learnings()?;
        let learning_list: Vec<&Learning> = learnings
            .values()
            .filter(|l| scope_filter.is_none() || Some(l.scope) == scope_filter)
            .filter(|l| source_id_filter.is_none() || Some(l.source_id.as_str()) == source_id_filter.as_deref())
            .collect();

        match serde_json::to_value(&learning_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_learning_reinforce(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_learning_reinforce()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut learnings = stores.write_learnings()?;
        let learning = match learnings.get_mut(&id) {
            Some(l) => l,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("learning", &id))),
        };

        let promotion = stores.config.strategy.promotion;
        learning.reinforce(&promotion);

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(learning.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let learning_json = match serde_json::to_value(&*learning) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("learning", &id));

        Ok(DaemonResponse::ok(req.id, learning_json))
    })
}

pub(super) fn handle_learning_contradict(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_learning_contradict()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut learnings = stores.write_learnings()?;
        let learning = match learnings.get_mut(&id) {
            Some(l) => l,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("learning", &id))),
        };

        let was_promoted = learning.promoted;
        learning.contradict();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(learning.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let learning_json = match serde_json::to_value(&*learning) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("learning", &id));
        if was_promoted {
            let _ = event_tx.send(DaemonEvent::learning_policy_contradicted(&id));
        }

        Ok(DaemonResponse::ok(req.id, learning_json))
    })
}

pub(super) fn handle_learning_promote(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_learning_promote()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut learnings = stores.write_learnings()?;
        let learning = match learnings.get_mut(&id) {
            Some(l) => l,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("learning", &id))),
        };

        learning.promote();
        learning.updated_at = crate::id::now_millis();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(learning.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let learning_json = match serde_json::to_value(&*learning) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("learning", &id));

        Ok(DaemonResponse::ok(req.id, learning_json))
    })
}

pub(super) fn handle_learning_demote(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_learning_demote()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut learnings = stores.write_learnings()?;
        let learning = match learnings.get_mut(&id) {
            Some(l) => l,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("learning", &id))),
        };

        learning.demote();
        learning.updated_at = crate::id::now_millis();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(learning.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let learning_json = match serde_json::to_value(&*learning) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("learning", &id));

        Ok(DaemonResponse::ok(req.id, learning_json))
    })
}

pub(super) fn handle_learning_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_learning_update()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut learnings = stores.write_learnings()?;
        let learning = match learnings.get_mut(&id) {
            Some(l) => l,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("learnings", &id))),
        };

        if let Some(content) = req.params.get("content").and_then(|v| v.as_str()) {
            learning.content = content.to_string();
        }
        if let Some(roles) = req.params.get("applicable_roles").and_then(|v| v.as_array()) {
            let parsed: Vec<Role> = roles
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect();
            learning.applicable_roles = if parsed.is_empty() { None } else { Some(parsed) };
        }
        if let Some(tags) = req.params.get("resource_tags").and_then(|v| v.as_array()) {
            learning.resource_tags = tags.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        learning.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(learning.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let learning_json = serde_json::to_value(&*learning)?;
        let _ = event_tx.send(DaemonEvent::record_updated("learnings", &id));
        Ok(DaemonResponse::ok(req.id, learning_json))
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
    use crate::domain::learning::Learning;
    use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
    use crate::worktree::manager::WorktreeManager;

    /// Helper: create a learning and return its id
    async fn create_learning(
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
                "learning.create",
                json!({
                    "source_id": "wi-123",
                    "scope": "work",
                    "content": "Always run tests"
                }),
            ),
        )
        .await;
        assert!(!resp.is_error());
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    // === Tests from mod.rs lines 4077-4595 ===

    #[tokio::test]
    async fn test_learning_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "learning.create",
            json!({
                "source_id": "wi-123",
                "scope": "work",
                "content": "Always run tests"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(retrieved.is_some());
        let learning = retrieved.unwrap();
        assert_eq!(learning.source_id, "wi-123");
        assert_eq!(learning.content, "Always run tests");
    }

    #[tokio::test]
    async fn test_learning_create_success() {
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
                "learning.create",
                json!({
                    "source_id": "wi-123",
                    "scope": "work",
                    "content": "Always run tests before committing"
                }),
            ),
        )
        .await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["source_id"], "wi-123");
        assert_eq!(result["scope"], "work");
        assert_eq!(result["content"], "Always run tests before committing");
        assert_eq!(result["reinforcements"], 0);
        assert!(!result["promoted"].as_bool().unwrap());
        assert_eq!(stores.learnings.read().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_learning_create_missing_source_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.create", json!({"scope": "global", "content": "insight"})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("source_id"));
    }

    #[tokio::test]
    async fn test_learning_create_missing_content() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.create", json!({"source_id": "wi-1", "scope": "global"})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("content"));
    }

    #[tokio::test]
    async fn test_learning_create_invalid_scope() {
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
                "learning.create",
                json!({"source_id": "wi-1", "scope": "invalid", "content": "test"}),
            ),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("scope"));
    }

    #[tokio::test]
    async fn test_learning_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        )
        .await;
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "learning");
    }

    // --- learning.get tests ---

    #[tokio::test]
    async fn test_learning_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.get", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["source_id"], "wi-123");
    }

    #[tokio::test]
    async fn test_learning_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.get", json!({"id": "nonexistent"})),
        )
        .await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_learning_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a learning (writes to both TaskStore and HashMap)
        let learning_id = create_learning(&stores, &tx, &wm, 50).await;

        // Remove from HashMap to prove get reads from TaskStore
        stores.learnings.write().unwrap().remove(&learning_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(51, "learning.get", json!({"id": learning_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req).await;
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["source_id"], "wi-123");
    }

    // --- learning.list tests ---

    #[tokio::test]
    async fn test_learning_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.list", json!(null)),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_learning_list_with_scope_filter() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create a work-scoped learning
        create_learning(&stores, &tx, &wm, 1).await;
        // Create a global-scoped learning
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "learning.create",
                json!({"source_id": "global", "scope": "global", "content": "global insight"}),
            ),
        )
        .await;

        // Filter by global scope
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.list", json!({"scope": "global"})),
        )
        .await;
        assert!(!resp.is_error());
        let list = resp.result.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["scope"], "global");
    }

    #[tokio::test]
    async fn test_learning_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a work-scoped learning (writes to both TaskStore and HashMap)
        create_learning(&stores, &tx, &wm, 1).await;
        // Create a global-scoped learning
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "learning.create",
                json!({"source_id": "global-src", "scope": "global", "content": "global insight"}),
            ),
        )
        .await;

        // Clear HashMap to prove list reads from TaskStore
        stores.learnings.write().unwrap().clear();

        // List all should still return both learnings via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "learning.list", json!(null)),
        )
        .await;
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list by scope works from TaskStore
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "learning.list", json!({"scope": "global"})),
        )
        .await;
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        assert_eq!(filtered_items.as_array().unwrap().len(), 1);
        assert_eq!(filtered_items[0]["scope"], "global");
    }

    // --- learning.reinforce tests ---

    #[tokio::test]
    async fn test_learning_reinforce() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.reinforce", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["reinforcements"], 1);

        // Reinforce again
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.reinforce", json!({"id": learning_id})),
        )
        .await;
        assert_eq!(resp2.result.unwrap()["reinforcements"], 2);
    }

    #[tokio::test]
    async fn test_learning_reinforce_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.reinforce", json!({"id": "nonexistent"})),
        )
        .await;
        assert!(resp.is_error());
    }

    // --- learning.contradict tests ---

    #[tokio::test]
    async fn test_learning_contradict() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.contradict", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["contradictions"], 1);
    }

    // --- learning.promote / demote tests ---

    #[tokio::test]
    async fn test_learning_promote_and_demote() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;

        // Promote
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert!(resp.result.unwrap()["promoted"].as_bool().unwrap());

        // Demote
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.demote", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp2.is_error());
        assert!(!resp2.result.unwrap()["promoted"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_learning_promote_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;
        let _ = rx.try_recv(); // consume create event

        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        )
        .await;
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.updated");
        assert_eq!(event.data["collection"], "learning");
    }

    #[tokio::test]
    async fn test_learning_reinforce_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.reinforce", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert_eq!(learning.unwrap().reinforcements, 1);
    }

    #[tokio::test]
    async fn test_learning_contradict_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.contradict", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert_eq!(learning.unwrap().contradictions, 1);
    }

    #[tokio::test]
    async fn test_learning_promote_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert!(learning.unwrap().promoted);
    }

    #[tokio::test]
    async fn test_learning_demote_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;

        // Promote first so we can demote
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        )
        .await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.demote", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert!(!learning.unwrap().promoted);
    }

    // === Tests from mod.rs lines 6384-6581 ===

    #[tokio::test]
    async fn test_handle_learning_update() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1).await;

        // Update content, applicable_roles, and resource_tags
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "learning.update",
                json!({
                    "id": learning_id,
                    "content": "Updated content",
                    "applicable_roles": ["Implementer", "Reviewer"],
                    "resource_tags": ["src/main.rs", "src/lib.rs"]
                }),
            ),
        )
        .await;
        assert!(!resp.is_error(), "learning.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["content"], "Updated content");
        assert_eq!(result["resource_tags"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_handle_learning_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.update", json!({"id": "nonexistent", "content": "x"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_learning_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.update", json!({"content": "x"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_learning_reinforce() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create learning via dispatch (persists to TaskStore)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        )
        .await;
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Reinforce it - exercises the TaskStore persist path
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.reinforce", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["reinforcements"], 1);
    }

    #[tokio::test]
    async fn test_handle_learning_contradict() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        )
        .await;
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.contradict", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["contradictions"], 1);
    }

    #[tokio::test]
    async fn test_handle_learning_promote() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        )
        .await;
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["promoted"], true);
    }

    #[tokio::test]
    async fn test_handle_learning_demote() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        )
        .await;
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Promote first
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());

        // Then demote
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.demote", json!({"id": learning_id})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["promoted"], false);
    }
}
