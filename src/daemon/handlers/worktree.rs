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

        match worktree_mgr.create_branch(&work_id, &base_ref) {
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::broadcast;

    use crate::daemon::context::Stores;
    use crate::daemon::handlers::dispatch;
    use crate::daemon::handlers::tests::{test_event_tx, test_integrator_config, test_stores, test_worktree_mgr};
    use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
    use crate::worktree::manager::WorktreeManager;

    async fn create_test_plan(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        )
        .await;
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    async fn create_test_spec(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String) {
        let plan_id = create_test_plan(stores, tx, wm).await;
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "spec.create", json!({"parent_id": plan_id, "title": "Parent Spec"})),
        )
        .await;
        let spec_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id)
    }

    async fn create_test_phase(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String, String) {
        let (plan_id, spec_id) = create_test_spec(stores, tx, wm).await;
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"parent_id": spec_id, "title": "Parent Phase", "order": 1}),
            ),
        )
        .await;
        let phase_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id, phase_id)
    }

    async fn create_test_work(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String) {
        let (_, _, phase_id) = create_test_phase(stores, tx, wm).await;
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"parent_id": phase_id, "title": "Parent WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        )
        .await;
        let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (phase_id, wi_id)
    }

    #[tokio::test]
    async fn test_worktree_create_missing_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.create", json!({})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("work_id"));
    }

    #[tokio::test]
    async fn test_worktree_create_work_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.create", json!({"work_id": "nonexistent"})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("not found"));
    }

    #[tokio::test]
    async fn test_worktree_create_validates_work_exists() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create a full hierarchy so work exists
        let (_, wi_id) = create_test_work(&stores, &tx, &wm).await;
        // This will fail at the git level (nonexistent repo path) but should
        // pass the work validation
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "worktree.create", json!({"work_id": wi_id})),
        )
        .await;
        // The error should be from git, not from "not found"
        assert!(resp.is_error());
        let msg = &resp.error.as_ref().unwrap().message;
        assert!(
            !msg.contains("not found"),
            "error should be from git, not from validation: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_worktree_list_returns_response() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // list on nonexistent repo will error, but it routes correctly
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.list", json!(null)),
        )
        .await;
        // Will be an error since the repo doesn't exist, but the method routes
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_worktree_cleanup_missing_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.cleanup", json!({})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("work_id"));
    }

    #[tokio::test]
    async fn test_worktree_cleanup_work_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.cleanup", json!({"work_id": "nonexistent"})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("not found"));
    }

    #[tokio::test]
    async fn test_worktree_refresh_missing_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.refresh", json!({})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("work_id"));
    }

    #[tokio::test]
    async fn test_worktree_refresh_nonexistent_worktree() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.refresh", json!({"work_id": "nonexistent"})),
        )
        .await;
        // Will error since worktree path doesn't exist
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_worktree_dispatch_routes_all_methods() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Verify all 4 worktree methods are routed (not method_not_found)
        for method in &[
            "worktree.create",
            "worktree.list",
            "worktree.cleanup",
            "worktree.refresh",
        ] {
            let resp = dispatch(
                &stores,
                &tx,
                &wm,
                &test_integrator_config(),
                DaemonRequest::new(1, *method, json!({})),
            )
            .await;
            // Even if they error, they should NOT be method_not_found (-32601)
            if resp.is_error() {
                assert_ne!(
                    resp.error.as_ref().unwrap().code,
                    -32601,
                    "method {} should be routed",
                    method
                );
            }
        }
    }
}
