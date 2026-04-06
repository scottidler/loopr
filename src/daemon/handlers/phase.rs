use std::sync::Arc;

use eyre::eyre;
use tokio::sync::broadcast;
use tracing::{debug, instrument};

use crate::domain::phase::{Phase, PhaseStatus};
use crate::domain::plan::HierarchyStatus;
use crate::domain::role::Role;
use crate::domain::transition::Transition;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

use super::common::check_validation_gate;
use super::{parse_optional_param, parse_required_param};

#[instrument(skip_all, fields(parent_id = ?req.params.get("parent_id")))]
pub(super) fn handle_phase_create(
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

        // Verify parent spec exists and is not in a terminal state
        {
            let specs = stores.read_specs()?;
            match specs.get(&parent_id) {
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &parent_id))),
                Some(spec) if matches!(spec.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Cannot create phase under {} spec '{}'",
                            spec.status(),
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
                .any(|p| p.parent_id == parent_id && p.status() == HierarchyStatus::Draft)
            {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(
                        "A Draft Phase already exists under this Spec; abandon it before creating a new one",
                    ),
                ));
            }
        }

        let phase = Phase::new(parent_id, title, description, order);
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

#[instrument(skip_all, fields(id = ?req.params.get("id")))]
pub(super) fn handle_phase_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

#[instrument(skip_all, fields(parent_id = ?req.params.get("parent_id")))]
pub(super) fn handle_phase_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        let parent_id_filter = req.params.get("parent_id").and_then(|v| v.as_str());

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let filters: Vec<Filter> = if let Some(sid) = parent_id_filter {
                vec![Filter {
                    field: "parent_id".to_string(),
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
            .filter(|p| parent_id_filter.is_none() || Some(p.parent_id.as_str()) == parent_id_filter)
            .collect();

        match serde_json::to_value(&phase_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

#[instrument(skip_all, fields(id = ?req.params.get("id"), target_status = ?req.params.get("target_status")))]
pub(super) fn handle_phase_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: PhaseStatus = match parse_required_param(&req, "target_status") {
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

        let mut phases = stores.write_phases()?;
        let phase = match phases.get_mut(&id) {
            Some(p) => p,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", &id))),
        };

        let from = phase.status();
        match from.validate_transition(target_status, role) {
            Err(e) => {
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
            "phase",
            &id,
            skip_validation,
            skip_reason,
        ) {
            return Ok(DaemonResponse::err(req.id, err));
        }

        phase.force_status(target_status);
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

#[instrument(skip_all, fields(id = ?req.params.get("id")))]
pub(super) fn handle_phase_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
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
    use crate::domain::phase::{Phase, PhaseStatus};
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

    /// Helper: create a plan + spec + phase and return (plan_id, spec_id, phase_id)
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

    // --- phase create tests ---

    #[tokio::test]
    async fn test_phase_create_rejects_duplicate_draft() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;
        // Create first Draft Phase - succeeds
        let req1 = DaemonRequest::new(
            1,
            "phase.create",
            json!({"parent_id": spec_id, "title": "Phase A", "order": 1}),
        );
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1).await;
        assert!(!resp1.is_error());

        // Create second Draft Phase under same Spec - rejected
        let req2 = DaemonRequest::new(
            2,
            "phase.create",
            json!({"parent_id": spec_id, "title": "Phase B", "order": 2}),
        );
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2).await;
        assert!(resp2.is_error());
        assert_eq!(resp2.error.unwrap().code, -32005);
    }

    #[tokio::test]
    async fn test_phase_create_rejects_complete_spec() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;

        // Transition spec: Draft -> Active -> Complete
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "spec.transition",
                json!({"id": spec_id, "target_status": "active", "role": "coordinator"}),
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
                "spec.transition",
                json!({"id": spec_id, "target_status": "complete", "role": "coordinator"}),
            ),
        )
        .await;

        let req = DaemonRequest::new(
            2,
            "phase.create",
            json!({"parent_id": spec_id, "title": "Phase Under Complete", "order": 1}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("complete spec"));
    }

    #[tokio::test]
    async fn test_phase_create_rejects_abandoned_spec() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;

        // Transition spec: Draft -> Abandoned
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "spec.transition",
                json!({"id": spec_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        )
        .await;

        let req = DaemonRequest::new(
            2,
            "phase.create",
            json!({"parent_id": spec_id, "title": "Phase Under Abandoned", "order": 1}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("abandoned spec"));
    }

    #[tokio::test]
    async fn test_phase_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;

        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({
                "parent_id": spec_id,
                "title": "Test Phase",
                "description": "A phase",
                "order": 1
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Phase");
        assert_eq!(result["parent_id"], spec_id);
        assert_eq!(result["status"], "draft");
        assert_eq!(result["order"], 1);
        assert_eq!(stores.phases.read().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_phase_create_missing_spec_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.create", json!({"title": "Phase"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("parent_id"));
    }

    #[tokio::test]
    async fn test_phase_create_spec_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.create", json!({"parent_id": "nonexistent", "title": "Phase"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_phase_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;
        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"parent_id": spec_id, "description": "no title"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[tokio::test]
    async fn test_phase_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;
        // Drain plan+spec create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(20, "phase.create", json!({"parent_id": spec_id, "title": "Phase"}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "phase");
    }

    #[tokio::test]
    async fn test_phase_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;
        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"parent_id": spec_id, "title": "Persisted Phase", "description": "desc", "order": 1}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let phase_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Phase> = store.get(&phase_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted Phase");
    }

    // --- phase get tests ---

    #[tokio::test]
    async fn test_phase_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"parent_id": spec_id, "title": "My Phase", "order": 3}),
            ),
        )
        .await;
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(21, "phase.get", json!({"id": phase_id})),
        )
        .await;
        assert!(!get_resp.is_error());
        let result = get_resp.result.unwrap();
        assert_eq!(result["title"], "My Phase");
        assert_eq!(result["order"], 3);
    }

    #[tokio::test]
    async fn test_phase_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[tokio::test]
    async fn test_phase_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;

        // Create a phase (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"parent_id": spec_id, "title": "TaskStore Phase", "order": 1}),
        );
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req).await;
        assert!(!create_resp.is_error());
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.phases.write().unwrap().remove(&phase_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(21, "phase.get", json!({"id": phase_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req).await;
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore Phase");
    }

    // --- phase list tests ---

    #[tokio::test]
    async fn test_phase_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_phase_list_filtered_by_spec_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (plan_id, spec_id_1) = create_test_spec(&stores, &tx, &wm).await;

        // Activate first spec so we can create a second Draft Spec (and phases under both)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                10,
                "spec.transition",
                json!({"id": spec_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        )
        .await;

        // Create a second spec under the same plan
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "spec.create", json!({"parent_id": plan_id, "title": "Spec 2"})),
        )
        .await;
        let spec_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create phases under different specs
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"parent_id": spec_id_1, "title": "Phase A", "order": 1}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"parent_id": spec_id_2, "title": "Phase B", "order": 1}),
            ),
        )
        .await;

        // List all - should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "phase.list", json!(null)),
        )
        .await;
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by spec_id_1 - should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "phase.list", json!({"parent_id": spec_id_1})),
        )
        .await;
        let phases = filtered_resp.result.unwrap();
        let arr = phases.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Phase A");
    }

    #[tokio::test]
    async fn test_phase_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (plan_id, spec_id_1) = create_test_spec(&stores, &tx, &wm).await;

        // Activate first spec so we can create a second Draft Spec
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                10,
                "spec.transition",
                json!({"id": spec_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        )
        .await;

        // Create a second spec under the same plan
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "spec.create", json!({"parent_id": plan_id, "title": "Spec 2"})),
        )
        .await;
        let spec_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create phases under different specs (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"parent_id": spec_id_1, "title": "Phase A", "order": 1}),
            ),
        )
        .await;
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"parent_id": spec_id_2, "title": "Phase B", "order": 1}),
            ),
        )
        .await;

        // Clear HashMap to prove list reads from TaskStore
        stores.phases.write().unwrap().clear();

        // List all should still return both phases via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "phase.list", json!(null)),
        )
        .await;
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(31, "phase.list", json!({"parent_id": spec_id_1}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req).await;
        assert!(!filtered_resp.is_error());
        let filtered_phases = filtered_resp.result.unwrap();
        let arr = filtered_phases.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Phase A");
    }

    // --- phase transition tests ---

    #[tokio::test]
    async fn test_phase_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;
        // Drain plan+spec create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(20, "phase.create", json!({"parent_id": spec_id, "title": "Phase"})),
        )
        .await;
        let _ = rx.try_recv(); // consume phase create event
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            21,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "phase");
        assert_eq!(event.data["from"], "draft");
        assert_eq!(event.data["to"], "active");
    }

    #[tokio::test]
    async fn test_phase_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(20, "phase.create", json!({"parent_id": spec_id, "title": "Phase"})),
        )
        .await;
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            21,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "complete",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[tokio::test]
    async fn test_phase_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(20, "phase.create", json!({"parent_id": spec_id, "title": "Phase"})),
        )
        .await;
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            21,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "active",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[tokio::test]
    async fn test_phase_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "phase.transition",
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
    async fn test_phase_transition_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm).await;

        // Create phase (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "phase.create",
                json!({"parent_id": spec_id, "title": "Transition Phase"}),
            ),
        )
        .await;
        assert!(!create_resp.is_error());
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft -> Active
        let req = DaemonRequest::new(
            3,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Phase> = store.get(&phase_id).unwrap();
        assert!(retrieved.is_some());
        let phase = retrieved.unwrap();
        assert_eq!(phase.status(), PhaseStatus::Active);
    }

    // --- phase validation gate tests ---

    #[tokio::test]
    async fn test_phase_transition_blocked_no_report_when_validator_enabled() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create parent plan -> spec -> phase
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
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Parent Spec"})),
        )
        .await;
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let phase_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "phase.create",
                json!({
                    "parent_id": spec_id, "title": "Gate Test Phase", "order": 1
                }),
            ),
        )
        .await;
        let phase_id = phase_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft -> Active without report - blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                4,
                "phase.transition",
                json!({
                    "id": phase_id,
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
    async fn test_phase_transition_skip_validation_override() {
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
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Parent Spec"})),
        )
        .await;
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let phase_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "phase.create",
                json!({
                    "parent_id": spec_id, "title": "Gate Test Phase", "order": 1
                }),
            ),
        )
        .await;
        let phase_id = phase_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft -> Active with skip_validation - should succeed
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                4,
                "phase.transition",
                json!({
                    "id": phase_id,
                    "target_status": "active",
                    "role": "coordinator",
                    "skip_validation": true
                }),
            ),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    // --- phase update tests ---

    #[tokio::test]
    async fn test_handle_phase_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm).await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "phase.update",
                json!({
                    "id": phase_id,
                    "title": "Updated Phase",
                    "description": "New desc",
                    "order": 5
                }),
            ),
        )
        .await;
        assert!(!resp.is_error(), "phase.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Phase");
        assert_eq!(result["order"], 5);
    }

    #[tokio::test]
    async fn test_handle_phase_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "phase.update", json!({"id": "nonexistent", "title": "x"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_phase_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "phase.update", json!({"title": "x"})),
        )
        .await;
        assert!(resp.is_error());
    }
}
