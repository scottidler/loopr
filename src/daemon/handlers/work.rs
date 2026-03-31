use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::domain::bundle::BundleStatus;
use crate::domain::plan::HierarchyStatus;
use crate::domain::role::Role;
use crate::domain::transition::validate_transition;
use crate::domain::work::{Work, WorkStatus, override_transitions, work_transitions};
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

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

        let target_status: WorkStatus = match req.params.get("target_status") {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(s) => s,
                Err(_) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::invalid_params("invalid target_status"),
                    ));
                }
            },
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("target_status is required"),
                ));
            }
        };

        let role: Role = match req.params.get("role") {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(r) => r,
                Err(_) => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("invalid role"))),
            },
            None => Role::Coordinator,
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
        let rules = if is_override { override_transitions() } else { work_transitions() };
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
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
