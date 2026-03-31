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
