use std::sync::Arc;

use eyre::eyre;
use log::debug;
use serde_json::json;
use tokio::sync::broadcast;

use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::worktree::manager::WorktreeManager;

use crate::daemon::context::Stores;
use crate::domain::work::Work;

pub(super) fn handle_worktree_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_worktree_create()");
        let work_id = match req.params.get("work_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("work_id is required"),
                ));
            }
        };

        let base_ref = req
            .params
            .get("base_ref")
            .and_then(|v| v.as_str())
            .unwrap_or("HEAD")
            .to_string();

        // Validate the work exists (TaskStore first, fallback to HashMap)
        {
            let found = if let Some(store) = &stores.store {
                store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .get::<Work>(&work_id)
                    .ok()
                    .is_some()
            } else {
                false
            };
            if !found {
                let works = stores.read_works()?;
                if !works.contains_key(&work_id) {
                    return Ok(DaemonResponse::err(req.id, RpcError::not_found("work", &work_id)));
                }
            }
        }

        // Check if worktree already exists before attempting git operations
        if worktree_mgr.exists(&work_id) {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params(&format!("worktree already exists for work {work_id}")),
            ));
        }

        match worktree_mgr.create(&work_id, &base_ref) {
            Ok(path) => {
                let _ = event_tx.send(DaemonEvent::new(
                    "worktree.created",
                    json!({ "work_id": work_id, "path": path.to_string_lossy() }),
                ));
                Ok(DaemonResponse::ok(
                    req.id,
                    json!({ "work_id": work_id, "path": path.to_string_lossy() }),
                ))
            }
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_worktree_list(worktree_mgr: &WorktreeManager, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_worktree_list()");
        match worktree_mgr.list() {
            Ok(worktrees) => match serde_json::to_value(&worktrees) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_worktree_cleanup(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_worktree_cleanup()");
        let work_id = match req.params.get("work_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("work_id is required"),
                ));
            }
        };

        // Validate the work exists (TaskStore first, fallback to HashMap)
        {
            let found = if let Some(store) = &stores.store {
                store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .get::<Work>(&work_id)
                    .ok()
                    .is_some()
            } else {
                false
            };
            if !found {
                let works = stores.read_works()?;
                if !works.contains_key(&work_id) {
                    return Ok(DaemonResponse::err(req.id, RpcError::not_found("work", &work_id)));
                }
            }
        }

        let path = worktree_mgr.worktree_path(&work_id);
        match worktree_mgr.cleanup(&work_id) {
            Ok(()) => {
                let _ = event_tx.send(DaemonEvent::new(
                    "worktree.cleaned",
                    json!({ "work_id": work_id, "path": path.to_string_lossy() }),
                ));
                Ok(DaemonResponse::ok(
                    req.id,
                    json!({ "work_id": work_id, "path": path.to_string_lossy(), "status": "cleaned" }),
                ))
            }
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_worktree_refresh(worktree_mgr: &WorktreeManager, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_worktree_refresh()");
        let work_id = match req.params.get("work_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("work_id is required"),
                ));
            }
        };

        let new_base_ref = req
            .params
            .get("new_base_ref")
            .and_then(|v| v.as_str())
            .unwrap_or("HEAD")
            .to_string();

        match worktree_mgr.refresh(&work_id, &new_base_ref) {
            Ok(()) => Ok(DaemonResponse::ok(
                req.id,
                json!({ "work_id": work_id, "status": "refreshed" }),
            )),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}
