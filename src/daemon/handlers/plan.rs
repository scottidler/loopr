use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::domain::plan::{HierarchyStatus, Plan, PlanStatus};
use crate::domain::role::Role;
use crate::domain::transition::Transition;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use crate::daemon::context::Stores;

use super::{parse_optional_param, parse_required_param};

use super::common::check_validation_gate;

pub(super) fn handle_plan_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_create()");
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
        let acceptance_criteria = req
            .params
            .get("acceptance_criteria")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if title.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("title is required"),
            ));
        }

        // Reject if a Draft Plan already exists — Coordinator must abandon the old one first
        {
            let plans = stores.read_plans()?;
            if plans.values().any(|p| p.status() == HierarchyStatus::Draft) {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("A Draft Plan already exists; abandon it before creating a new one"),
                ));
            }
        }

        let plan = Plan::new(title, description, acceptance_criteria);
        let plan_json = match serde_json::to_value(&plan) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = plan.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(plan.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_plans()?.insert(id.clone(), plan);
        let _ = event_tx.send(DaemonEvent::record_created("plan", &id));

        Ok(DaemonResponse::ok(req.id, plan_json))
    })
}

pub(super) fn handle_plan_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Plan>(id)
            {
                Ok(Some(plan)) => {
                    return match serde_json::to_value(&plan) {
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

        let plans = stores.read_plans()?;
        match plans.get(id) {
            Some(plan) => match serde_json::to_value(plan) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", id))),
        }
    })
}

pub(super) fn handle_plan_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_list()");
        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Plan>(&[])
            {
                Ok(plans) => {
                    return match serde_json::to_value(&plans) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let plans = stores.read_plans()?;
        let plan_list: Vec<&Plan> = plans.values().collect();
        match serde_json::to_value(&plan_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_plan_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: PlanStatus = match parse_required_param(&req, "target_status") {
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

        let mut plans = stores.write_plans()?;
        let plan = match plans.get_mut(&id) {
            Some(p) => p,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &id))),
        };

        let from = plan.status();
        match from.validate_transition(target_status, role) {
            Err(e) => {
                let _ = event_tx.send(DaemonEvent::transition_rejected(
                    "plans",
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
            "plan",
            &id,
            skip_validation,
            skip_reason,
        ) {
            return Ok(DaemonResponse::err(req.id, err));
        }

        plan.force_status(target_status);
        plan.updated_at = crate::id::now_millis();
        let plan_clone = plan.clone();
        drop(plans);

        // Persist transition to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(plan_clone.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let plan_json = match serde_json::to_value(&plan_clone) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        debug!(
            "[transition] plan.{}: {:?} -> {:?} by {}",
            id, from, target_status, role
        );
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "plan",
            &id,
            &from.to_string(),
            &target_status.to_string(),
            &role.to_string(),
        ));

        Ok(DaemonResponse::ok(req.id, plan_json))
    })
}

pub(super) fn handle_plan_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_plan_update()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut plans = stores.write_plans()?;
        let plan = match plans.get_mut(&id) {
            Some(p) => p,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("plans", &id))),
        };

        if let Some(title) = req.params.get("title").and_then(|v| v.as_str()) {
            plan.title = title.to_string();
        }
        if let Some(desc) = req.params.get("description").and_then(|v| v.as_str()) {
            plan.description = desc.to_string();
        }
        if let Some(criteria) = req.params.get("acceptance_criteria").and_then(|v| v.as_str()) {
            plan.acceptance_criteria = criteria.to_string();
        }
        plan.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(plan.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let plan_json = serde_json::to_value(&*plan)?;
        let _ = event_tx.send(DaemonEvent::record_updated("plans", &id));
        Ok(DaemonResponse::ok(req.id, plan_json))
    })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::daemon::handlers::dispatch;
    use crate::daemon::handlers::tests::{
        test_event_tx, test_integrator_config, test_stores, test_stores_with_taskstore, test_stores_with_validator,
        test_stores_with_validator_strictness, test_worktree_mgr,
    };
    use crate::domain::plan::{Plan, PlanStatus};
    use crate::domain::validation::ValidationReport;
    use crate::ipc::protocol::DaemonRequest;

    // --- plan.create tests ---

    #[test]
    fn test_plan_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "plan.create",
            json!({
                "title": "Test Plan",
                "description": "A test",
                "acceptance_criteria": "It works"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Plan");
        assert_eq!(result["status"], "draft");
        // Should be stored
        assert_eq!(stores.plans.read().unwrap().len(), 1);
    }

    #[test]
    fn test_plan_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "plan.create", json!({"description": "no title"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_plan_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let req = DaemonRequest::new(1, "plan.create", json!({"title": "Plan"}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "plan");
    }

    #[test]
    fn test_plan_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "plan.create",
            json!({"title": "Persisted Plan", "description": "desc"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let plan_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Plan> = store.get(&plan_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted Plan");
    }

    #[test]
    fn test_plan_create_rejects_duplicate_draft() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create first Draft Plan - succeeds
        let req1 = DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"}));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());

        // Create second Draft Plan - rejected
        let req2 = DaemonRequest::new(2, "plan.create", json!({"title": "Plan B"}));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(resp2.is_error());
        assert_eq!(resp2.error.unwrap().code, -32005); // precondition_failed
    }

    // --- plan.get tests ---

    #[test]
    fn test_plan_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create a plan first
        let create_req = DaemonRequest::new(1, "plan.create", json!({"title": "My Plan"}));
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Get it
        let get_req = DaemonRequest::new(2, "plan.get", json!({"id": plan_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My Plan");
    }

    #[test]
    fn test_plan_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "plan.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_plan_get_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "plan.get", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("id"));
    }

    #[test]
    fn test_plan_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(
            1,
            "plan.create",
            json!({"title": "TaskStore Plan", "description": "persistent"}),
        );
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        assert!(!create_resp.is_error());
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.plans.write().unwrap().remove(&plan_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(2, "plan.get", json!({"id": plan_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore Plan");
    }

    // --- plan.list tests ---

    #[test]
    fn test_plan_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "plan.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let plans = resp.result.unwrap();
        assert!(plans.is_array());
        assert_eq!(plans.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_plan_list_with_plans() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create first plan and abandon it so a second Draft can be created
        let resp1 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"})),
        );
        let plan_a_id = resp1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                10,
                "plan.transition",
                json!({"id": plan_a_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "plan.create", json!({"title": "Plan B"})),
        );

        let req = DaemonRequest::new(3, "plan.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let plans = resp.result.unwrap();
        assert_eq!(plans.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_plan_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create first plan and abandon it so a second Draft can be created
        let resp1 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"})),
        );
        let plan_a_id = resp1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                10,
                "plan.transition",
                json!({"id": plan_a_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "plan.create", json!({"title": "Plan B"})),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.plans.write().unwrap().clear();

        // List should still return both plans via TaskStore
        let req = DaemonRequest::new(3, "plan.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let plans = resp.result.unwrap();
        assert_eq!(plans.as_array().unwrap().len(), 2);
    }

    // --- plan.transition tests ---

    #[test]
    fn test_plan_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();

        // Create plan
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let _ = rx.try_recv(); // consume create event
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft -> Active
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Check event was broadcast
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["from"], "draft");
        assert_eq!(event.data["to"], "active");
    }

    #[test]
    fn test_plan_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try to skip Draft -> Complete (invalid: must go through Active)
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "complete",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_plan_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition plans
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_plan_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "plan.transition",
            json!({
                "id": "nonexistent",
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_plan_transition_missing_params() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Missing id
        let req = DaemonRequest::new(1, "plan.transition", json!({"target_status": "active"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());

        // Missing target_status
        let req = DaemonRequest::new(2, "plan.transition", json!({"id": "x"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
    }

    #[test]
    fn test_plan_transition_default_role_coordinator() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // No role specified - defaults to Coordinator, which is valid for hierarchy transitions
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_plan_transition_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create plan (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Transition Plan"})),
        );
        assert!(!create_resp.is_error());
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft -> Active
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Plan> = store.get(&plan_id).unwrap();
        assert!(retrieved.is_some());
        let plan = retrieved.unwrap();
        assert_eq!(plan.status(), PlanStatus::Active);
    }

    // --- plan validation gate tests ---

    #[test]
    fn test_plan_transition_blocked_no_report_when_validator_enabled() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Draft -> Active without any validation report - should be blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.as_ref().unwrap().code, -32003);
        assert!(resp.error.unwrap().message.contains("validator.validate"));
    }

    #[test]
    fn test_plan_transition_allowed_with_pass_report() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Insert a passing validation report into TaskStore
        let report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Pass,
            vec![],
            "All criteria met".to_string(),
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
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_plan_transition_allowed_with_warn_report() {
        let (_dir, stores) =
            test_stores_with_validator_strictness(crate::config::ValidatorStrictness::AllowAmbiguityWithFlags);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Insert a Warn validation report
        let report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Warn,
            vec![],
            "Passes with warnings".to_string(),
            "test-model".to_string(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Draft -> Active should succeed (Warn allows transition)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_plan_transition_blocked_with_fail_report() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Insert a Fail validation report
        let report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Fail,
            vec![],
            "Missing criteria".to_string(),
            "test-model".to_string(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Draft -> Active should be blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32003);
    }

    #[test]
    fn test_plan_transition_skip_validation_override() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft -> Active with skip_validation=true - should succeed even without report
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator",
                    "skip_validation": true
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_plan_transition_no_gate_when_validator_disabled() {
        // test_stores_with_taskstore has no validator - gate should not apply
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "No Gate Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft -> Active should succeed without any validation report
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    // --- plan.update tests ---

    #[test]
    fn test_handle_plan_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.update",
                json!({
                    "id": plan_id,
                    "title": "Updated Plan",
                    "description": "New desc",
                    "acceptance_criteria": "New criteria"
                }),
            ),
        );
        assert!(!resp.is_error(), "plan.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Plan");
        assert_eq!(result["description"], "New desc");
        assert_eq!(result["acceptance_criteria"], "New criteria");
    }

    #[test]
    fn test_handle_plan_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.update", json!({"id": "nonexistent", "title": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_plan_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.update", json!({"title": "x"})),
        );
        assert!(resp.is_error());
    }
}
