use std::sync::Arc;

use eyre::eyre;
use tokio::sync::broadcast;
use tracing::{debug, instrument};

use crate::domain::markdown::write_doc_markdown;
use crate::domain::plan::HierarchyStatus;
use crate::domain::role::Role;
use crate::domain::spec::{Spec, SpecStatus};
use crate::domain::transition::Transition;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

use super::common::check_validation_gate;
use super::{parse_optional_param, parse_required_param};

#[instrument(skip_all, fields(parent_id = ?req.params.get("parent_id")))]
pub(super) fn handle_spec_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let parent_id = match req.params.get("parent_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("parent_id is required"),
                ));
            }
        };

        // Verify parent plan exists and is not in a terminal state
        {
            let plans = stores.read_plans()?;
            match plans.get(&parent_id) {
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &parent_id))),
                Some(plan) if matches!(plan.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Cannot create spec under {} plan '{}'",
                            plan.status(),
                            parent_id
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
                .any(|s| s.parent_id == parent_id && s.status() == HierarchyStatus::Draft)
            {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(
                        "A Draft Spec already exists under this Plan; abandon it before creating a new one",
                    ),
                ));
            }
        }

        let order = req.params.get("order").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let spec = Spec::new(parent_id, title, order);
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

        stores.write_specs()?.insert(id.clone(), spec.clone());
        if let Err(e) = write_doc_markdown(&stores.config.project.repo_path, &spec) {
            tracing::warn!("docs/loopr write failed for {}: {}", id, e);
        }
        let _ = event_tx.send(DaemonEvent::record_created("spec", &id));

        Ok(DaemonResponse::ok(req.id, spec_json))
    })
}

#[instrument(skip_all, fields(id = ?req.params.get("id")))]
pub(super) fn handle_spec_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

#[instrument(skip_all, fields(parent_id = ?req.params.get("parent_id")))]
pub(super) fn handle_spec_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        let parent_id_filter = req.params.get("parent_id").and_then(|v| v.as_str());

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let filters: Vec<Filter> = if let Some(pid) = parent_id_filter {
                vec![Filter {
                    field: "parent_id".to_string(),
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
            .filter(|s| parent_id_filter.is_none() || Some(s.parent_id.as_str()) == parent_id_filter)
            .collect();

        match serde_json::to_value(&spec_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

#[instrument(skip_all, fields(id = ?req.params.get("id"), target_status = ?req.params.get("target_status")))]
pub(super) fn handle_spec_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: SpecStatus = match parse_required_param(&req, "target_status") {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        };

        let role: Role = match parse_optional_param(&req, "role", Role::Coordinator) {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
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

        let from = spec.status();
        match from.validate_transition(target_status, role) {
            Err(e) => {
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
            Ok(Transition::Unchanged) => {
                return Ok(DaemonResponse::ok(req.id, serde_json::Value::Null));
            }
            Ok(Transition::Changed) => {}
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

        spec.force_status(target_status);
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
        if let Err(e) = write_doc_markdown(&stores.config.project.repo_path, &spec_clone) {
            tracing::warn!("docs/loopr write failed for {}: {}", id, e);
        }
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

#[instrument(skip_all, fields(id = ?req.params.get("id")))]
pub(super) fn handle_spec_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
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
        if let Err(e) = write_doc_markdown(&stores.config.project.repo_path, &*spec) {
            tracing::warn!("docs/loopr write failed for {}: {}", id, e);
        }
        let _ = event_tx.send(DaemonEvent::record_updated("specs", &id));
        Ok(DaemonResponse::ok(req.id, spec_json))
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
        test_event_tx, test_integrator_config, test_stores, test_stores_with_taskstore, test_stores_with_validator,
        test_worktree_mgr,
    };
    use crate::domain::spec::{Spec, SpecStatus};
    use crate::domain::validation::ValidationReport;
    use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
    use crate::worktree::manager::WorktreeManager;

    /// Helper: create a plan and return its id
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

    /// Helper: create a plan + spec and return (plan_id, spec_id)
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

    // --- spec.create tests ---

    #[tokio::test]
    async fn test_spec_create_success() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({
                "parent_id": plan_id,
                "title": "Test Spec",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Spec");
        assert_eq!(result["parent_id"], plan_id);
        assert_eq!(result["status"], "draft");
        assert_eq!(stores.specs.read().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_spec_create_missing_plan_id() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.create", json!({"title": "Spec"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("parent_id"));
    }

    #[tokio::test]
    async fn test_spec_create_plan_not_found() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.create", json!({"parent_id": "nonexistent", "title": "Spec"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_spec_create_missing_title() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;
        let req = DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[tokio::test]
    async fn test_spec_create_broadcasts_event() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;
        let _ = rx.try_recv(); // consume plan create event

        let req = DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Spec"}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "spec");
    }

    #[tokio::test]
    async fn test_spec_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;
        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"parent_id": plan_id, "title": "Persisted Spec"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let spec_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Spec> = store.get(&spec_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted Spec");
    }

    // --- parent status validation tests ---

    #[tokio::test]
    async fn test_spec_create_rejects_complete_plan() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;

        // Transition plan: Draft -> Active -> Complete
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "complete", "role": "coordinator"}),
            ),
        )
        .await;

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"parent_id": plan_id, "title": "Spec Under Complete"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("complete plan"));
    }

    #[tokio::test]
    async fn test_spec_create_rejects_abandoned_plan() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;

        // Transition plan: Draft -> Abandoned
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        )
        .await;

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"parent_id": plan_id, "title": "Spec Under Abandoned"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("abandoned plan"));
    }

    #[tokio::test]
    async fn test_spec_create_rejects_duplicate_draft() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;
        // Create first Draft Spec - succeeds
        let req1 = DaemonRequest::new(1, "spec.create", json!({"parent_id": plan_id, "title": "Spec A"}));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1).await;
        assert!(!resp1.is_error());

        // Create second Draft Spec under same Plan - rejected
        let req2 = DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Spec B"}));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2).await;
        assert!(resp2.is_error());
        assert_eq!(resp2.error.unwrap().code, -32005);
    }

    // --- spec.get tests ---

    #[tokio::test]
    async fn test_spec_get_success() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "My Spec"})),
        )
        .await;
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "spec.get", json!({"id": spec_id})),
        )
        .await;
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My Spec");
    }

    #[tokio::test]
    async fn test_spec_get_not_found() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_spec_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;

        // Create a spec (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"parent_id": plan_id, "title": "TaskStore Spec"}),
        );
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req).await;
        assert!(!create_resp.is_error());
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.specs.write().unwrap().remove(&spec_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(3, "spec.get", json!({"id": spec_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req).await;
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore Spec");
    }

    // --- spec.list tests ---

    #[tokio::test]
    async fn test_spec_list_empty() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_spec_list_filtered_by_plan_id() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id_1 = create_test_plan(&stores, &tx, &wm).await;

        // Activate first plan so we can create a second Draft Plan
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                9,
                "plan.transition",
                json!({"id": plan_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        )
        .await;

        // Create a second plan
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "plan.create", json!({"title": "Plan 2"})),
        )
        .await;
        let plan_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create specs under different plans
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id_1, "title": "Spec A"})),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "spec.create", json!({"parent_id": plan_id_2, "title": "Spec B"})),
        )
        .await;

        // List all - should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "spec.list", json!(null)),
        )
        .await;
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by plan_id_1 - should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(5, "spec.list", json!({"parent_id": plan_id_1})),
        )
        .await;
        let specs = filtered_resp.result.unwrap();
        let arr = specs.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Spec A");
    }

    #[tokio::test]
    async fn test_spec_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan first
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan X"})),
        )
        .await;
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create first spec, abandon it, then create second
        let spec_a_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Spec A"})),
        )
        .await;
        let spec_a_id = spec_a_resp.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "spec.transition",
                json!({"id": spec_a_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "spec.create", json!({"parent_id": plan_id, "title": "Spec B"})),
        )
        .await;

        // Clear HashMap to prove list reads from TaskStore
        stores.specs.write().unwrap().clear();

        // List should still return both specs via TaskStore
        let req = DaemonRequest::new(4, "spec.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let specs = resp.result.unwrap();
        assert_eq!(specs.as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(5, "spec.list", json!({"parent_id": plan_id}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req).await;
        assert!(!filtered_resp.is_error());
        let filtered_specs = filtered_resp.result.unwrap();
        assert_eq!(filtered_specs.as_array().unwrap().len(), 2);
    }

    // --- spec.transition tests ---

    #[tokio::test]
    async fn test_spec_transition_draft_to_active() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;
        let _ = rx.try_recv(); // consume plan create event

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Spec"})),
        )
        .await;
        let _ = rx.try_recv(); // consume spec create event
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "spec");
        assert_eq!(event.data["from"], "draft");
        assert_eq!(event.data["to"], "active");
    }

    #[tokio::test]
    async fn test_spec_transition_invalid_skip_state() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Spec"})),
        )
        .await;
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "complete",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[tokio::test]
    async fn test_spec_transition_wrong_role() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Spec"})),
        )
        .await;
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "active",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[tokio::test]
    async fn test_spec_transition_not_found() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "spec.transition",
            json!({
                "id": "nonexistent",
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_spec_transition_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm).await;

        // Create spec (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "spec.create",
                json!({"parent_id": plan_id, "title": "Transition Spec"}),
            ),
        )
        .await;
        assert!(!create_resp.is_error());
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft -> Active
        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Spec> = store.get(&spec_id).unwrap();
        assert!(retrieved.is_some());
        let spec = retrieved.unwrap();
        assert_eq!(spec.status(), SpecStatus::Active);
    }

    // --- spec validation gate tests ---

    #[tokio::test]
    async fn test_spec_transition_blocked_no_report_when_validator_enabled() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create parent plan
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        )
        .await;
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create spec
        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "spec.create",
                json!({"parent_id": plan_id, "title": "Gate Test Spec"}),
            ),
        )
        .await;
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft -> Active without report - blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "spec.transition",
                json!({
                    "id": spec_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        )
        .await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32003);
    }

    #[tokio::test]
    async fn test_spec_transition_allowed_with_pass_report() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        )
        .await;
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "spec.create",
                json!({"parent_id": plan_id, "title": "Gate Test Spec"}),
            ),
        )
        .await;
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Insert passing report
        let report = ValidationReport::new(
            "specs".to_string(),
            spec_id.clone(),
            crate::domain::validation::ValidationVerdict::Pass,
            vec![],
            "ok".to_string(),
            "test-model".to_string(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Draft -> Active should succeed
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "spec.transition",
                json!({
                    "id": spec_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    // --- spec.update tests ---

    #[tokio::test]
    async fn test_handle_spec_update_success() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "spec.update",
                json!({
                    "id": spec_id,
                    "title": "Updated Spec",
                }),
            ),
        )
        .await;
        assert!(!resp.is_error(), "spec.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Spec");
    }

    #[tokio::test]
    async fn test_handle_spec_update_not_found() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "spec.update", json!({"id": "nonexistent", "title": "x"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_spec_update_missing_id() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "spec.update", json!({"title": "x"})),
        )
        .await;
        assert!(resp.is_error());
    }
}
