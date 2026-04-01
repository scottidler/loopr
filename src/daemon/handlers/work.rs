use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::domain::bundle::BundleStatus;
use crate::domain::plan::HierarchyStatus;
use crate::domain::role::Role;
use crate::domain::transition::Transition;
use crate::domain::work::{Work, WorkStatus};
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

use super::{parse_optional_param, parse_required_param};

/// BFS cycle detection: returns true if adding `dependencies` to `new_id` would
/// create a cycle in the dependency graph.
pub(super) fn detect_dependency_cycle(works: &HashMap<String, Work>, new_id: &str, dependencies: &[String]) -> bool {
    let mut visited = HashSet::new();
    let mut queue: VecDeque<&str> = dependencies.iter().map(|s| s.as_str()).collect();

    while let Some(current) = queue.pop_front() {
        if current == new_id {
            return true;
        }
        if visited.insert(current)
            && let Some(wi) = works.get(current)
        {
            for dep in &wi.dependencies {
                queue.push_back(dep.as_str());
            }
        }
    }
    false
}

pub(super) fn handle_work_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_work_create()");
        let phase_id = match req.params.get("phase_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("phase_id is required"),
                ));
            }
        };

        // Verify parent phase exists and is not in a terminal state
        {
            let phases = stores.read_phases()?;
            match phases.get(&phase_id) {
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", &phase_id))),
                Some(phase) if matches!(phase.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Cannot create work under {} phase '{}'",
                            phase.status, phase_id
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

        if title.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("title is required"),
            ));
        }

        // Duplicate detection: reject work with same title in same phase (unless Abandoned)
        {
            let works = stores.read_works()?;
            let duplicate = works.values().find(|wi| {
                wi.phase_id == phase_id
                    && wi.title.to_lowercase() == title.to_lowercase()
                    && !matches!(wi.status, WorkStatus::Abandoned)
            });
            if let Some(dup) = duplicate {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(&format!(
                        "Duplicate work '{}' already exists in phase {} with status {} (ID: {})",
                        title, phase_id, dup.status, dup.id
                    )),
                ));
            }
        }

        let resource_tags: Vec<String> = req
            .params
            .get("resource_tags")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // #17: Work must have at least one resource_tag
        if resource_tags.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::precondition_failed("Work must have at least one resource_tag"),
            ));
        }

        let acceptance_criteria: Vec<String> = req
            .params
            .get("acceptance_criteria")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let dependencies: Vec<String> = req
            .params
            .get("dependencies")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // #16: Validate dependencies — skip unknown IDs with warning instead of rejecting
        let dependencies = if !dependencies.is_empty() {
            let works = stores.read_works()?;
            let mut valid_deps = Vec::new();
            for dep_id in &dependencies {
                if dep_id.starts_with("batch:") {
                    // Batch references (e.g., "batch:0") can't be resolved here — skip with warning
                    log::warn!(
                        "Work creation: batch dependency '{}' cannot be resolved at handler level, skipping",
                        dep_id
                    );
                } else if works.contains_key(dep_id) {
                    valid_deps.push(dep_id.clone());
                } else {
                    log::warn!("Work creation: dependency '{}' not found, skipping", dep_id);
                }
            }
            valid_deps
        } else {
            dependencies
        };

        let mut work = Work::new(phase_id, title, description);
        work.resource_tags = resource_tags;
        work.acceptance_criteria = acceptance_criteria.clone();
        work.dependencies = dependencies;

        // Reject circular dependencies (including self-references)
        if !work.dependencies.is_empty() {
            let works = stores.read_works()?;
            if detect_dependency_cycle(&works, &work.id, &work.dependencies) {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(
                        "Circular dependency detected: adding these dependencies would create a cycle",
                    ),
                ));
            }
        }

        let id = work.id.clone();

        // Auto-promote to Ready if acceptance_criteria are provided.
        // Draft→Ready is always valid for Coordinator role.
        if !acceptance_criteria.is_empty() {
            work.status = WorkStatus::Ready;
            work.updated_at = crate::id::now_millis();
        }

        // Persist to TaskStore
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(work.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let wi_json = match serde_json::to_value(&work) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        stores.write_works()?.insert(id.clone(), work);
        let _ = event_tx.send(DaemonEvent::record_created("work", &id));

        Ok(DaemonResponse::ok(req.id, wi_json))
    })
}

pub(super) fn handle_work_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_work_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Work>(id)
            {
                Ok(Some(wi)) => {
                    return match serde_json::to_value(&wi) {
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

        let works = stores.read_works()?;
        match works.get(id) {
            Some(wi) => match serde_json::to_value(wi) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("work", id))),
        }
    })
}

pub(super) fn handle_work_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_work_list()");
        let phase_id_filter = req.params.get("phase_id").and_then(|v| v.as_str());

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let filters: Vec<Filter> = if let Some(pid) = phase_id_filter {
                vec![Filter {
                    field: "phase_id".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(pid.to_string()),
                }]
            } else {
                vec![]
            };
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Work>(&filters)
            {
                Ok(works) => {
                    return match serde_json::to_value(&works) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let works = stores.read_works()?;
        let wi_list: Vec<&Work> = works
            .values()
            .filter(|wi| phase_id_filter.is_none() || Some(wi.phase_id.as_str()) == phase_id_filter)
            .collect();

        match serde_json::to_value(&wi_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_work_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_work_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: WorkStatus = match parse_required_param(&req, "target_status") {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        };

        let role: Role = match parse_optional_param(&req, "role", Role::Coordinator) {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        };

        let is_override = req.params.get("override").and_then(|v| v.as_bool()).unwrap_or(false);
        let override_reason = req
            .params
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("no reason provided")
            .to_string();

        let mut works = stores.write_works()?;
        let wi = match works.get_mut(&id) {
            Some(w) => w,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("work", &id))),
        };

        let from = wi.status;
        let result = if is_override {
            from.validate_override(target_status, role)
        } else {
            from.validate_transition(target_status, role)
        };
        match result {
            Err(e) => {
                let _ = event_tx.send(DaemonEvent::transition_rejected(
                    "works",
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

        // Allow setting assignee via transition params
        if let Some(assignee) = req.params.get("assignee").and_then(|v| v.as_str()) {
            wi.assignee = Some(assignee.to_string());
        }

        // #13: Assignee required for InProgress/InReview
        if matches!(target_status, WorkStatus::InProgress | WorkStatus::InReview) && wi.assignee.is_none() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::precondition_failed("Work must have an assignee before transitioning to InProgress/InReview"),
            ));
        }

        // #14: acceptance_criteria required for Ready
        if target_status == WorkStatus::Ready && wi.acceptance_criteria.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::precondition_failed("Work must have acceptance_criteria before transitioning to Ready"),
            ));
        }

        // #15: InReview requires active Bundle (not Rejected/Merged/Superseded)
        if target_status == WorkStatus::InReview {
            let bundles = stores.read_bundles()?;
            let has_active_bundle = bundles.values().any(|b| {
                b.work_id == wi.id
                    && !matches!(
                        b.status,
                        BundleStatus::Rejected | BundleStatus::Merged | BundleStatus::Superseded
                    )
            });
            if !has_active_bundle {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("Work cannot move to InReview without an active Bundle"),
                ));
            }
        }

        wi.status = target_status;
        wi.updated_at = crate::id::now_millis();
        let wi_clone = wi.clone();
        drop(works);

        // Persist transition to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(wi_clone.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let wi_json = match serde_json::to_value(&wi_clone) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        debug!(
            "[transition] work.{}: {:?} -> {:?} by {}",
            id, from, target_status, role
        );
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "work",
            &id,
            &from.to_string(),
            &target_status.to_string(),
            &role.to_string(),
        ));

        if is_override {
            log::warn!(
                "OVERRIDE: Work {} transitioned {:?} → {:?} by Coordinator (reason: {})",
                id,
                from,
                target_status,
                override_reason
            );
            let _ = event_tx.send(DaemonEvent::new(
                "work.override_transition",
                serde_json::json!({
                    "work_id": id,
                    "from": format!("{:?}", from),
                    "to": format!("{:?}", target_status),
                    "reason": override_reason,
                }),
            ));
        }

        Ok(DaemonResponse::ok(req.id, wi_json))
    })
}

pub(super) fn handle_work_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_work_update()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Pre-validate dependency cycle before taking mutable borrow
        if let Some(deps) = req.params.get("dependencies").and_then(|v| v.as_array()) {
            let new_deps: Vec<String> = deps.iter().filter_map(|v| v.as_str().map(String::from)).collect();
            if !new_deps.is_empty() {
                let works = stores.read_works()?;
                // Exclude self to avoid false positives from stale edges
                let check_works: HashMap<String, Work> = works
                    .iter()
                    .filter(|(k, _)| k.as_str() != id)
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if detect_dependency_cycle(&check_works, &id, &new_deps) {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(
                            "Circular dependency detected: updating dependencies would create a cycle",
                        ),
                    ));
                }
            }
        }

        let mut works = stores.write_works()?;
        let wi = match works.get_mut(&id) {
            Some(w) => w,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("works", &id))),
        };

        if let Some(title) = req.params.get("title").and_then(|v| v.as_str()) {
            wi.title = title.to_string();
        }
        if let Some(desc) = req.params.get("description").and_then(|v| v.as_str()) {
            wi.description = desc.to_string();
        }
        if let Some(assignee) = req.params.get("assignee").and_then(|v| v.as_str()) {
            wi.assignee = Some(assignee.to_string());
        }
        if let Some(tags) = req.params.get("resource_tags").and_then(|v| v.as_array()) {
            wi.resource_tags = tags.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(criteria) = req.params.get("acceptance_criteria").and_then(|v| v.as_array()) {
            wi.acceptance_criteria = criteria.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(deps) = req.params.get("dependencies").and_then(|v| v.as_array()) {
            wi.dependencies = deps.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        wi.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(wi.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let wi_json = serde_json::to_value(&*wi)?;
        let _ = event_tx.send(DaemonEvent::record_updated("works", &id));
        Ok(DaemonResponse::ok(req.id, wi_json))
    })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::broadcast;

    use crate::daemon::context::Stores;
    use crate::daemon::handlers::dispatch;
    use crate::daemon::handlers::tests::{
        test_event_tx, test_integrator_config, test_stores, test_stores_with_taskstore, test_worktree_mgr,
    };
    use crate::domain::work::{Work, WorkStatus};
    use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
    use crate::worktree::manager::WorktreeManager;

    use super::detect_dependency_cycle;

    /// Helper: create a plan and return its id
    fn create_test_plan(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>, wm: &WorktreeManager) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    /// Helper: create a plan + spec and return (plan_id, spec_id)
    fn create_test_spec(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String) {
        let plan_id = create_test_plan(stores, tx, wm);
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "spec.create", json!({"plan_id": plan_id, "title": "Parent Spec"})),
        );
        let spec_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id)
    }

    /// Helper: create a plan + spec + phase and return (plan_id, spec_id, phase_id)
    fn create_test_phase(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String, String) {
        let (plan_id, spec_id) = create_test_spec(stores, tx, wm);
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Parent Phase", "order": 1}),
            ),
        );
        let phase_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id, phase_id)
    }

    /// Helper: create plan + spec + phase + work and return (phase_id, work_id)
    fn create_test_work(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String) {
        let (_, _, phase_id) = create_test_phase(stores, tx, wm);
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "Parent WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (phase_id, wi_id)
    }

    /// Helper: create a work item with optional dependencies
    fn create_work(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
        phase_id: &str,
        title: &str,
        deps: &[&str],
    ) -> String {
        let mut params = json!({
            "phase_id": phase_id,
            "title": title,
            "description": "test",
            "resource_tags": ["src/"],
            "acceptance_criteria": ["pass"],
        });
        if !deps.is_empty() {
            params["dependencies"] = json!(deps);
        }
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "work.create", params),
        );
        assert!(!resp.is_error(), "work.create failed: {:?}", resp.error);
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    // --- work create rejection tests ---

    #[test]
    fn test_work_create_rejects_complete_phase() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Transition phase: Draft -> Active -> Complete
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "phase.transition",
                json!({"id": phase_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "phase.transition",
                json!({"id": phase_id, "target_status": "complete", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "work.create",
            json!({"phase_id": phase_id, "title": "Work Under Complete", "resource_tags": ["src/"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("complete phase"));
    }

    #[test]
    fn test_work_create_rejects_abandoned_phase() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Transition phase: Draft -> Abandoned
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "phase.transition",
                json!({"id": phase_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "work.create",
            json!({"phase_id": phase_id, "title": "Work Under Abandoned", "resource_tags": ["src/"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("abandoned phase"));
    }

    // --- work create tests ---

    #[test]
    fn test_work_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            30,
            "work.create",
            json!({
                "phase_id": phase_id,
                "title": "Implement auth",
                "description": "Add JWT signing"
            , "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Implement auth");
        assert_eq!(result["phase_id"], phase_id);
        // Auto-promoted to Ready because acceptance_criteria were provided
        assert_eq!(result["status"], "Ready");
        assert_eq!(stores.works.read().unwrap().len(), 1);
    }

    #[test]
    fn test_work_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            30,
            "work.create",
            json!({"phase_id": phase_id, "title": "Persisted WI", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Work> = store.get(&wi_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted WI");
    }

    #[test]
    fn test_work_create_missing_phase_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "work.create",
            json!({"title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("phase_id"));
    }

    #[test]
    fn test_work_create_phase_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "work.create",
            json!({"phase_id": "nonexistent", "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            30,
            "work.create",
            json!({"phase_id": phase_id, "description": "no title", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_work_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);
        // Drain plan+spec+phase create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(
            30,
            "work.create",
            json!({"phase_id": phase_id, "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "work");
    }

    // --- work get tests ---

    #[test]
    fn test_work_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "My WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "work.get", json!({"id": wi_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My WI");
    }

    #[test]
    fn test_work_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "work.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Create a work item (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(
            30,
            "work.create",
            json!({"phase_id": phase_id, "title": "TaskStore WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        assert!(!create_resp.is_error());
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.works.write().unwrap().remove(&wi_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(31, "work.get", json!({"id": wi_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore WI");
    }

    // --- work list tests ---

    #[test]
    fn test_work_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "work.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_work_list_filtered_by_phase_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id, phase_id_1) = create_test_phase(&stores, &tx, &wm);

        // Activate first phase so we can create a second Draft Phase
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                15,
                "phase.transition",
                json!({"id": phase_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        );

        // Create a second phase under the same spec
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Phase 2", "order": 2}),
            ),
        );
        let phase_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create work items under different phases
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id_1, "title": "WI A", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                31,
                "work.create",
                json!({"phase_id": phase_id_2, "title": "WI B", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );

        // List all - should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(40, "work.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by phase_id_1 - should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(41, "work.list", json!({"phase_id": phase_id_1})),
        );
        let items = filtered_resp.result.unwrap();
        let arr = items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "WI A");
    }

    #[test]
    fn test_work_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id, phase_id_1) = create_test_phase(&stores, &tx, &wm);

        // Activate first phase so we can create a second Draft Phase
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                15,
                "phase.transition",
                json!({"id": phase_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        );

        // Create a second phase under the same spec
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Phase 2", "order": 2}),
            ),
        );
        let phase_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create work items under different phases (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id_1, "title": "WI A", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                31,
                "work.create",
                json!({"phase_id": phase_id_2, "title": "WI B", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.works.write().unwrap().clear();

        // List all should still return both work items via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(40, "work.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(41, "work.list", json!({"phase_id": phase_id_1}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        let arr = filtered_items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "WI A");
    }

    // --- work transition tests ---

    #[test]
    fn test_work_transition_draft_to_ready() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);
        // Drain plan+spec+phase create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        // With acceptance_criteria, WI is auto-promoted to Ready on creation
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let _ = rx.try_recv(); // consume work create event
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Already Ready - transition to InProgress (with assignee, required by precondition)
        let req = DaemonRequest::new(
            31,
            "work.transition",
            json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "coordinator",
                "assignee": "agent-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "InProgress");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "work");
        assert_eq!(event.data["from"], "Ready");
        assert_eq!(event.data["to"], "InProgress");
    }

    #[test]
    fn test_work_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Ready -> Done (invalid: must go through InProgress, InReview, Integrated)
        let req = DaemonRequest::new(
            31,
            "work.transition",
            json!({
                "id": wi_id,
                "target_status": "Done",
                "role": "coordinator"
            , "assignee": "agent-1"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_work_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition Ready -> InProgress (only Coordinator can)
        let req = DaemonRequest::new(
            31,
            "work.transition",
            json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_work_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "work.transition",
            json!({
                "id": "nonexistent",
                "target_status": "Ready"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_transition_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Create work item (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "work.create",
                json!({"phase_id": phase_id, "title": "Transition WI", "description": "Test", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        assert!(!create_resp.is_error());
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Already Ready via auto-promotion (acceptance_criteria present) - transition to InProgress
        let req = DaemonRequest::new(
            3,
            "work.transition",
            json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "coordinator",
                "assignee": "agent-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "InProgress");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Work> = store.get(&wi_id).unwrap();
        assert!(retrieved.is_some());
        let wi = retrieved.unwrap();
        assert_eq!(wi.status, WorkStatus::InProgress);
    }

    // --- work update tests ---

    #[test]
    fn test_handle_work_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, wi_id) = create_test_work(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "work.update",
                json!({
                    "id": wi_id,
                    "title": "Updated Work",
                    "description": "New desc",
                    "assignee": "agent-1",
                    "resource_tags": ["src/lib.rs"],
                    "acceptance_criteria": ["tests pass"],
                    "dependencies": ["dep-1"]
                }),
            ),
        );
        assert!(!resp.is_error(), "work.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Work");
        assert_eq!(result["description"], "New desc");
        assert_eq!(result["assignee"], "agent-1");
        assert_eq!(result["resource_tags"].as_array().unwrap().len(), 1);
        assert_eq!(result["acceptance_criteria"].as_array().unwrap().len(), 1);
        assert_eq!(result["dependencies"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_handle_work_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "work.update", json!({"id": "nonexistent", "title": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_work_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "work.update", json!({"title": "x"})),
        );
        assert!(resp.is_error());
    }

    // --- dependency cycle detection unit tests ---

    #[test]
    fn test_detect_cycle_self_reference() {
        let mut works = HashMap::new();
        let w = Work::new("ph-1".into(), "A".into(), "desc".into());
        let id = w.id.clone();
        works.insert(id.clone(), w);
        // A depends on itself
        assert!(detect_dependency_cycle(&works, &id, std::slice::from_ref(&id)));
    }

    #[test]
    fn test_detect_cycle_direct() {
        let mut works = HashMap::new();
        let mut a = Work::new("ph-1".into(), "A".into(), "desc".into());
        let b = Work::new("ph-1".into(), "B".into(), "desc".into());
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        // A depends on B
        a.dependencies = vec![b_id.clone()];
        works.insert(a_id.clone(), a);
        works.insert(b_id.clone(), b);
        // Creating C with deps [A] is fine
        assert!(!detect_dependency_cycle(&works, "wk-new", std::slice::from_ref(&a_id)));
        // But if B tries to depend on A, it's a cycle
        assert!(detect_dependency_cycle(&works, &b_id, std::slice::from_ref(&a_id)));
    }

    #[test]
    fn test_detect_cycle_transitive() {
        let mut works = HashMap::new();
        let mut a = Work::new("ph-1".into(), "A".into(), "desc".into());
        let mut b = Work::new("ph-1".into(), "B".into(), "desc".into());
        let c = Work::new("ph-1".into(), "C".into(), "desc".into());
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        let c_id = c.id.clone();
        // A -> B -> C
        a.dependencies = vec![b_id.clone()];
        b.dependencies = vec![c_id.clone()];
        works.insert(a_id.clone(), a);
        works.insert(b_id.clone(), b);
        works.insert(c_id.clone(), c);
        // C trying to depend on A creates transitive cycle
        assert!(detect_dependency_cycle(&works, &c_id, std::slice::from_ref(&a_id)));
    }

    #[test]
    fn test_detect_cycle_valid_chain() {
        let mut works = HashMap::new();
        let a = Work::new("ph-1".into(), "A".into(), "desc".into());
        let mut b = Work::new("ph-1".into(), "B".into(), "desc".into());
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        b.dependencies = vec![a_id.clone()];
        works.insert(a_id, a);
        works.insert(b_id.clone(), b);
        // C depends on B (linear chain A <- B <- C) - no cycle
        assert!(!detect_dependency_cycle(&works, "wk-c", &[b_id]));
    }

    #[test]
    fn test_detect_cycle_diamond_accepted() {
        let mut works = HashMap::new();
        let d = Work::new("ph-1".into(), "D".into(), "desc".into());
        let mut b = Work::new("ph-1".into(), "B".into(), "desc".into());
        let mut c = Work::new("ph-1".into(), "C".into(), "desc".into());
        let d_id = d.id.clone();
        let b_id = b.id.clone();
        let c_id = c.id.clone();
        // B -> D, C -> D (diamond: A -> B -> D, A -> C -> D)
        b.dependencies = vec![d_id.clone()];
        c.dependencies = vec![d_id.clone()];
        works.insert(d_id, d);
        works.insert(b_id.clone(), b);
        works.insert(c_id.clone(), c);
        // A depends on both B and C - diamond, no cycle
        assert!(!detect_dependency_cycle(&works, "wk-a", &[b_id, c_id]));
    }

    // --- dependency cycle handler-level tests ---

    #[test]
    fn test_self_referencing_dependency_rejected_via_handler() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Create A first (no deps)
        let a_id = create_work(&stores, &tx, &wm, &phase_id, "A", &[]);

        // Try to update A to depend on itself
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "work.update", json!({"id": a_id, "dependencies": [a_id]})),
        );
        assert!(resp.is_error(), "expected error for self-referencing dependency");
        assert!(
            resp.error.as_ref().unwrap().message.contains("Circular dependency"),
            "expected cycle error, got: {:?}",
            resp.error
        );
    }

    #[test]
    fn test_direct_cycle_rejected_at_update() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        let a_id = create_work(&stores, &tx, &wm, &phase_id, "A", &[]);
        let b_id = create_work(&stores, &tx, &wm, &phase_id, "B", &[&a_id]);

        // Update A to depend on B - creates cycle A -> B -> A
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "work.update", json!({"id": a_id, "dependencies": [b_id]})),
        );
        assert!(resp.is_error(), "expected error for direct cycle");
        assert!(
            resp.error.as_ref().unwrap().message.contains("Circular dependency"),
            "expected cycle error, got: {:?}",
            resp.error
        );
    }

    #[test]
    fn test_valid_chain_accepted_via_handler() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        let a_id = create_work(&stores, &tx, &wm, &phase_id, "A", &[]);
        let b_id = create_work(&stores, &tx, &wm, &phase_id, "B", &[&a_id]);
        let c_id = create_work(&stores, &tx, &wm, &phase_id, "C", &[&b_id]);

        // Verify all were created successfully (linear chain)
        assert!(!a_id.is_empty());
        assert!(!b_id.is_empty());
        assert!(!c_id.is_empty());
    }
}
