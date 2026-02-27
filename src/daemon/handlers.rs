use std::process::Command;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;

use crate::agents::{AgentSession, AgentStatus, AgentType};
use crate::config::IntegratorConfig;
use crate::domain::bundle::{Bundle, BundleStatus, bundle_transitions};
use crate::domain::learning::{Learning, LearningScope};
use crate::domain::lock::Lock;
use crate::domain::phase::{Phase, PhaseStatus};
use crate::domain::plan::{HierarchyStatus, Plan, PlanStatus, hierarchy_transitions};
use crate::domain::role::Role;
use crate::domain::spec::{Spec, SpecStatus};
use crate::domain::tick::{Tick, TickStatus, tick_transitions};
use crate::domain::transition::validate_transition;
use crate::domain::validation::{ValidationReport, ValidationVerdict};
use crate::domain::work_item::{WorkItem, WorkItemStatus, work_item_transitions};
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::worktree::manager::WorktreeManager;

use taskstore::{Filter, FilterOp, IndexValue, Record};

use super::context::Stores;

/// Check the validation gate for Draft → Active transitions.
/// Returns `Some(RpcError)` if the gate blocks the transition, `None` if allowed.
/// Gate only applies when:
/// 1. Validator is enabled (stores.validator is Some)
/// 2. Transition is Draft → Active
/// 3. skip_validation param is not true
fn check_validation_gate(
    stores: &Arc<Stores>,
    from: HierarchyStatus,
    target: HierarchyStatus,
    collection: &str,
    id: &str,
    skip_validation: bool,
) -> Option<RpcError> {
    // Gate only applies to Draft → Active
    if from != HierarchyStatus::Draft || target != HierarchyStatus::Active {
        return None;
    }

    // Gate only applies when validator is enabled
    stores.validator.as_ref()?;

    // Coordinator can skip validation with explicit flag
    if skip_validation {
        return None;
    }

    // Check for a passing ValidationReport in TaskStore
    if let Some(store) = &stores.store {
        let store = store.lock().unwrap();
        let reports: Vec<ValidationReport> = store
            .list(&[Filter {
                field: "target_id".into(),
                op: FilterOp::Eq,
                value: IndexValue::String(id.to_string()),
            }])
            .unwrap_or_default();

        // Find the latest report (highest updated_at)
        let latest = reports.iter().max_by_key(|r| r.created_at);

        match latest {
            Some(report) => {
                if report.verdict == ValidationVerdict::Fail {
                    return Some(RpcError::validation_required(collection, id));
                }
                // Pass or Warn → allowed
                None
            }
            None => {
                // No report exists → block
                Some(RpcError::validation_required(collection, id))
            }
        }
    } else {
        // No TaskStore → no gate (shouldn't happen when validator is enabled, but be safe)
        None
    }
}

/// Dispatch an IPC request to the appropriate handler.
/// This is the central routing function for all daemon request handling.
pub fn dispatch(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    match req.method.as_str() {
        "system.handshake" => handle_handshake(req),
        "system.init" => handle_system_init(stores, req),
        "system.status" => handle_status(stores, req),
        "system.shutdown" => handle_shutdown(event_tx, req),
        "plan.create" => handle_plan_create(stores, event_tx, req),
        "plan.get" => handle_plan_get(stores, req),
        "plan.list" => handle_plan_list(stores, req),
        "plan.transition" => handle_plan_transition(stores, event_tx, req),
        "spec.create" => handle_spec_create(stores, event_tx, req),
        "spec.get" => handle_spec_get(stores, req),
        "spec.list" => handle_spec_list(stores, req),
        "spec.transition" => handle_spec_transition(stores, event_tx, req),
        "phase.create" => handle_phase_create(stores, event_tx, req),
        "phase.get" => handle_phase_get(stores, req),
        "phase.list" => handle_phase_list(stores, req),
        "phase.transition" => handle_phase_transition(stores, event_tx, req),
        "work_item.create" => handle_work_item_create(stores, event_tx, req),
        "work_item.get" => handle_work_item_get(stores, req),
        "work_item.list" => handle_work_item_list(stores, req),
        "work_item.transition" => handle_work_item_transition(stores, event_tx, req),
        "bundle.create" => handle_bundle_create(stores, event_tx, req),
        "bundle.get" => handle_bundle_get(stores, req),
        "bundle.list" => handle_bundle_list(stores, req),
        "bundle.transition" => handle_bundle_transition(stores, event_tx, req),
        "tick.create" => handle_tick_create(stores, event_tx, req),
        "tick.get" => handle_tick_get(stores, req),
        "tick.list" => handle_tick_list(stores, req),
        "tick.transition" => handle_tick_transition(stores, event_tx, req),
        "learning.create" => handle_learning_create(stores, event_tx, req),
        "learning.get" => handle_learning_get(stores, req),
        "learning.list" => handle_learning_list(stores, req),
        "learning.reinforce" => handle_learning_reinforce(stores, event_tx, req),
        "learning.contradict" => handle_learning_contradict(stores, event_tx, req),
        "learning.promote" => handle_learning_promote(stores, event_tx, req),
        "learning.demote" => handle_learning_demote(stores, event_tx, req),
        "lock.create" => handle_lock_create(stores, event_tx, req),
        "lock.get" => handle_lock_get(stores, req),
        "lock.list" => handle_lock_list(stores, req),
        "lock.release" => handle_lock_release(stores, event_tx, req),
        "lock.expire" => handle_lock_expire(stores, event_tx, req),
        "worktree.create" => handle_worktree_create(stores, event_tx, worktree_mgr, req),
        "worktree.list" => handle_worktree_list(worktree_mgr, req),
        "worktree.cleanup" => handle_worktree_cleanup(stores, event_tx, worktree_mgr, req),
        "worktree.refresh" => handle_worktree_refresh(worktree_mgr, req),
        "integrator.validate" => handle_integrator_validate(stores, event_tx, integrator_config, req),
        "integrator.publish" => handle_integrator_publish(stores, event_tx, integrator_config, req),
        "validator.validate" => handle_validator_validate(stores, req),
        "validator.report" => handle_validator_report(stores, req),
        "validator.reports" => handle_validator_reports(stores, req),
        "tool.list" => handle_tool_list(stores, req),
        "agent.start" => handle_agent_start(stores, event_tx, worktree_mgr, req),
        "agent.stop" => handle_agent_stop(stores, event_tx, req),
        "agent.pause" => handle_agent_pause(stores, event_tx, req),
        "agent.resume" => handle_agent_resume(stores, event_tx, req),
        "agent.status" => handle_agent_status(stores, req),
        "agent.list" => handle_agent_list(stores, req),
        _ => DaemonResponse::err(req.id, RpcError::method_not_found(&req.method)),
    }
}

fn handle_handshake(req: DaemonRequest) -> DaemonResponse {
    DaemonResponse::ok(
        req.id,
        json!({
            "server_version": env!("CARGO_PKG_VERSION"),
            "protocol": "ndjson/1"
        }),
    )
}

fn handle_system_init(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let store_arc = match &stores.store {
        Some(s) => s,
        None => {
            return DaemonResponse::err(req.id, RpcError::internal("TaskStore not initialized"));
        }
    };

    // Install git merge driver and .gitattributes (best-effort)
    let git_hooks_ok = {
        let store = store_arc.lock().unwrap();
        match store.install_git_hooks() {
            Ok(()) => true,
            Err(e) => {
                log::warn!("Failed to install git hooks (non-fatal): {}", e);
                false
            }
        }
    };

    // Return the list of collection names
    let collections = vec![
        Plan::collection_name(),
        Spec::collection_name(),
        Phase::collection_name(),
        WorkItem::collection_name(),
        Bundle::collection_name(),
        Tick::collection_name(),
        Learning::collection_name(),
        Lock::collection_name(),
        AgentSession::collection_name(),
    ];

    DaemonResponse::ok(
        req.id,
        json!({ "collections": collections, "git_hooks_installed": git_hooks_ok }),
    )
}

fn handle_status(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let plans = stores.plans.read().unwrap().len();
    let specs = stores.specs.read().unwrap().len();
    let phases = stores.phases.read().unwrap().len();
    let work_items = stores.work_items.read().unwrap().len();
    let bundles = stores.bundles.read().unwrap().len();
    let ticks = stores.ticks.read().unwrap().len();
    let learnings = stores.learnings.read().unwrap().len();
    let locks = stores.locks.read().unwrap().len();
    let agent_sessions = stores.agent_sessions.read().unwrap().len();

    // TaskStore stats (when available)
    let taskstore_stats = if let Some(store) = &stores.store {
        let s = store.lock().unwrap();
        let ts_plans = s.list::<Plan>(&[]).map(|v| v.len()).unwrap_or(0);
        let ts_specs = s.list::<Spec>(&[]).map(|v| v.len()).unwrap_or(0);
        let ts_phases = s.list::<Phase>(&[]).map(|v| v.len()).unwrap_or(0);
        let ts_work_items = s.list::<WorkItem>(&[]).map(|v| v.len()).unwrap_or(0);
        let ts_bundles = s.list::<Bundle>(&[]).map(|v| v.len()).unwrap_or(0);
        let ts_ticks = s.list::<Tick>(&[]).map(|v| v.len()).unwrap_or(0);
        let ts_learnings = s.list::<Learning>(&[]).map(|v| v.len()).unwrap_or(0);
        let ts_locks = s.list::<Lock>(&[]).map(|v| v.len()).unwrap_or(0);
        json!({
            "enabled": true,
            "counts": {
                "plans": ts_plans,
                "specs": ts_specs,
                "phases": ts_phases,
                "work_items": ts_work_items,
                "bundles": ts_bundles,
                "ticks": ts_ticks,
                "learnings": ts_learnings,
                "locks": ts_locks,
            }
        })
    } else {
        json!({ "enabled": false })
    };

    DaemonResponse::ok(
        req.id,
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "counts": {
                "plans": plans,
                "specs": specs,
                "phases": phases,
                "work_items": work_items,
                "bundles": bundles,
                "ticks": ticks,
                "learnings": learnings,
                "locks": locks,
                "agent_sessions": agent_sessions,
            },
            "taskstore": taskstore_stats,
        }),
    )
}

fn handle_shutdown(event_tx: &broadcast::Sender<DaemonEvent>, req: DaemonRequest) -> DaemonResponse {
    // Broadcast a shutdown event so the accept loop can pick it up
    let _ = event_tx.send(DaemonEvent::new("system.shutdown", json!({})));
    DaemonResponse::ok(req.id, json!({ "status": "shutting_down" }))
}

fn handle_plan_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
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
        return DaemonResponse::err(req.id, RpcError::invalid_params("title is required"));
    }

    let plan = Plan::new(title, description, acceptance_criteria);
    let plan_json = match serde_json::to_value(&plan) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = plan.id.clone();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().create(plan.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    stores.plans.write().unwrap().insert(id.clone(), plan);
    let _ = event_tx.send(DaemonEvent::record_created("plan", &id));

    DaemonResponse::ok(req.id, plan_json)
}

fn handle_plan_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<Plan>(id) {
            Ok(Some(plan)) => {
                return match serde_json::to_value(&plan) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let plans = stores.plans.read().unwrap();
    match plans.get(id) {
        Some(plan) => match serde_json::to_value(plan) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("plan", id)),
    }
}

fn handle_plan_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().list::<Plan>(&[]) {
            Ok(plans) => {
                return match serde_json::to_value(&plans) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let plans = stores.plans.read().unwrap();
    let plan_list: Vec<&Plan> = plans.values().collect();
    match serde_json::to_value(&plan_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_plan_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: PlanStatus = match req.params.get("target_status") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(s) => s,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid target_status")),
        },
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("target_status is required")),
    };

    let role: Role = match req.params.get("role") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid role")),
        },
        None => Role::Coordinator,
    };

    let skip_validation = req
        .params
        .get("skip_validation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut plans = stores.plans.write().unwrap();
    let plan = match plans.get_mut(&id) {
        Some(p) => p,
        None => return DaemonResponse::err(req.id, RpcError::not_found("plan", &id)),
    };

    let from = plan.status;
    let rules = hierarchy_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    // Validation gate: Draft → Active requires passing validation report
    if let Some(err) = check_validation_gate(stores, from, target_status, "plan", &id, skip_validation) {
        return DaemonResponse::err(req.id, err);
    }

    plan.status = target_status;
    plan.updated_at = crate::id::now_millis();

    // Persist transition to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(plan.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let plan_json = match serde_json::to_value(&*plan) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "plan",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, plan_json)
}

// --- Spec handlers ---

fn handle_spec_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let plan_id = match req.params.get("plan_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("plan_id is required")),
    };

    // Verify parent plan exists
    if !stores.plans.read().unwrap().contains_key(&plan_id) {
        return DaemonResponse::err(req.id, RpcError::not_found("plan", &plan_id));
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
        return DaemonResponse::err(req.id, RpcError::invalid_params("title is required"));
    }

    let spec = Spec::new(plan_id, title, description);
    let spec_json = match serde_json::to_value(&spec) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = spec.id.clone();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().create(spec.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    stores.specs.write().unwrap().insert(id.clone(), spec);
    let _ = event_tx.send(DaemonEvent::record_created("spec", &id));

    DaemonResponse::ok(req.id, spec_json)
}

fn handle_spec_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<Spec>(id) {
            Ok(Some(spec)) => {
                return match serde_json::to_value(&spec) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let specs = stores.specs.read().unwrap();
    match specs.get(id) {
        Some(spec) => match serde_json::to_value(spec) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("spec", id)),
    }
}

fn handle_spec_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let plan_id_filter = req.params.get("plan_id").and_then(|v| v.as_str());

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        let filters: Vec<Filter> = if let Some(pid) = plan_id_filter {
            vec![Filter {
                field: "plan_id".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(pid.to_string()),
            }]
        } else {
            vec![]
        };
        match store.lock().unwrap().list::<Spec>(&filters) {
            Ok(specs) => {
                return match serde_json::to_value(&specs) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let specs = stores.specs.read().unwrap();
    let spec_list: Vec<&Spec> = specs
        .values()
        .filter(|s| plan_id_filter.is_none() || Some(s.plan_id.as_str()) == plan_id_filter)
        .collect();

    match serde_json::to_value(&spec_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_spec_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: SpecStatus = match req.params.get("target_status") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(s) => s,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid target_status")),
        },
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("target_status is required")),
    };

    let role: Role = match req.params.get("role") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid role")),
        },
        None => Role::Coordinator,
    };

    let skip_validation = req
        .params
        .get("skip_validation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut specs = stores.specs.write().unwrap();
    let spec = match specs.get_mut(&id) {
        Some(s) => s,
        None => return DaemonResponse::err(req.id, RpcError::not_found("spec", &id)),
    };

    let from = spec.status;
    let rules = hierarchy_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    // Validation gate: Draft → Active requires passing validation report
    if let Some(err) = check_validation_gate(stores, from, target_status, "spec", &id, skip_validation) {
        return DaemonResponse::err(req.id, err);
    }

    spec.status = target_status;
    spec.updated_at = crate::id::now_millis();

    // Persist transition to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(spec.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let spec_json = match serde_json::to_value(&*spec) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "spec",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, spec_json)
}

// --- Phase handlers ---

fn handle_phase_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let spec_id = match req.params.get("spec_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("spec_id is required")),
    };

    // Verify parent spec exists
    if !stores.specs.read().unwrap().contains_key(&spec_id) {
        return DaemonResponse::err(req.id, RpcError::not_found("spec", &spec_id));
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
        return DaemonResponse::err(req.id, RpcError::invalid_params("title is required"));
    }

    let phase = Phase::new(spec_id, title, description, order);
    let phase_json = match serde_json::to_value(&phase) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = phase.id.clone();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().create(phase.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    stores.phases.write().unwrap().insert(id.clone(), phase);
    let _ = event_tx.send(DaemonEvent::record_created("phase", &id));

    DaemonResponse::ok(req.id, phase_json)
}

fn handle_phase_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<Phase>(id) {
            Ok(Some(phase)) => {
                return match serde_json::to_value(&phase) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let phases = stores.phases.read().unwrap();
    match phases.get(id) {
        Some(phase) => match serde_json::to_value(phase) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("phase", id)),
    }
}

fn handle_phase_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let spec_id_filter = req.params.get("spec_id").and_then(|v| v.as_str());

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        let filters: Vec<Filter> = if let Some(sid) = spec_id_filter {
            vec![Filter {
                field: "spec_id".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(sid.to_string()),
            }]
        } else {
            vec![]
        };
        match store.lock().unwrap().list::<Phase>(&filters) {
            Ok(phases) => {
                return match serde_json::to_value(&phases) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let phases = stores.phases.read().unwrap();
    let phase_list: Vec<&Phase> = phases
        .values()
        .filter(|p| spec_id_filter.is_none() || Some(p.spec_id.as_str()) == spec_id_filter)
        .collect();

    match serde_json::to_value(&phase_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_phase_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: PhaseStatus = match req.params.get("target_status") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(s) => s,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid target_status")),
        },
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("target_status is required")),
    };

    let role: Role = match req.params.get("role") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid role")),
        },
        None => Role::Coordinator,
    };

    let skip_validation = req
        .params
        .get("skip_validation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut phases = stores.phases.write().unwrap();
    let phase = match phases.get_mut(&id) {
        Some(p) => p,
        None => return DaemonResponse::err(req.id, RpcError::not_found("phase", &id)),
    };

    let from = phase.status;
    let rules = hierarchy_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    // Validation gate: Draft → Active requires passing validation report
    if let Some(err) = check_validation_gate(stores, from, target_status, "phase", &id, skip_validation) {
        return DaemonResponse::err(req.id, err);
    }

    phase.status = target_status;
    phase.updated_at = crate::id::now_millis();

    // Persist transition to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(phase.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let phase_json = match serde_json::to_value(&*phase) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "phase",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, phase_json)
}

// --- WorkItem handlers ---

fn handle_work_item_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let phase_id = match req.params.get("phase_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("phase_id is required")),
    };

    // Verify parent phase exists
    if !stores.phases.read().unwrap().contains_key(&phase_id) {
        return DaemonResponse::err(req.id, RpcError::not_found("phase", &phase_id));
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
        return DaemonResponse::err(req.id, RpcError::invalid_params("title is required"));
    }

    let work_item = WorkItem::new(phase_id, title, description);
    let wi_json = match serde_json::to_value(&work_item) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = work_item.id.clone();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().create(work_item.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    stores.work_items.write().unwrap().insert(id.clone(), work_item);
    let _ = event_tx.send(DaemonEvent::record_created("work_item", &id));

    DaemonResponse::ok(req.id, wi_json)
}

fn handle_work_item_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<WorkItem>(id) {
            Ok(Some(wi)) => {
                return match serde_json::to_value(&wi) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let work_items = stores.work_items.read().unwrap();
    match work_items.get(id) {
        Some(wi) => match serde_json::to_value(wi) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("work_item", id)),
    }
}

fn handle_work_item_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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
        match store.lock().unwrap().list::<WorkItem>(&filters) {
            Ok(work_items) => {
                return match serde_json::to_value(&work_items) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let work_items = stores.work_items.read().unwrap();
    let wi_list: Vec<&WorkItem> = work_items
        .values()
        .filter(|wi| phase_id_filter.is_none() || Some(wi.phase_id.as_str()) == phase_id_filter)
        .collect();

    match serde_json::to_value(&wi_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_work_item_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: WorkItemStatus = match req.params.get("target_status") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(s) => s,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid target_status")),
        },
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("target_status is required")),
    };

    let role: Role = match req.params.get("role") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid role")),
        },
        None => Role::Coordinator,
    };

    let mut work_items = stores.work_items.write().unwrap();
    let wi = match work_items.get_mut(&id) {
        Some(w) => w,
        None => return DaemonResponse::err(req.id, RpcError::not_found("work_item", &id)),
    };

    let from = wi.status;
    let rules = work_item_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    wi.status = target_status;
    wi.updated_at = crate::id::now_millis();

    // Persist transition to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(wi.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let wi_json = match serde_json::to_value(&*wi) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "work_item",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, wi_json)
}

// --- Bundle handlers ---

fn handle_bundle_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let work_item_id = match req.params.get("work_item_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("work_item_id is required")),
    };

    // Verify parent work item exists
    if !stores.work_items.read().unwrap().contains_key(&work_item_id) {
        return DaemonResponse::err(req.id, RpcError::not_found("work_item", &work_item_id));
    }

    let branch_name = req
        .params
        .get("branch_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if branch_name.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("branch_name is required"));
    }

    let base_tick_id = req
        .params
        .get("base_tick_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Staleness guard: reject if base_tick_id is behind the latest Published Tick
    let latest_published = find_latest_published_tick(stores);
    match (&base_tick_id, &latest_published) {
        // Published tick exists but bundle has no base_tick_id
        (None, Some(latest)) => {
            let _ = event_tx.send(DaemonEvent::bundle_rejected_stale(&work_item_id, "(none)", &latest.id));
            return DaemonResponse::err(req.id, RpcError::stale_bundle("(none)", &latest.id));
        }
        // Published tick exists and bundle's base_tick_id doesn't match it
        (Some(base_id), Some(latest)) if base_id != &latest.id => {
            let _ = event_tx.send(DaemonEvent::bundle_rejected_stale(&work_item_id, base_id, &latest.id));
            return DaemonResponse::err(req.id, RpcError::stale_bundle(base_id, &latest.id));
        }
        // No published tick and no base_tick_id: bootstrap case, OK
        // base_tick_id matches latest published: OK
        _ => {}
    }

    let claims = req
        .params
        .get("claims")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let bundle = Bundle::new(work_item_id, base_tick_id, branch_name, claims);
    let bundle_json = match serde_json::to_value(&bundle) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = bundle.id.clone();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().create(bundle.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    stores.bundles.write().unwrap().insert(id.clone(), bundle);
    let _ = event_tx.send(DaemonEvent::record_created("bundle", &id));

    DaemonResponse::ok(req.id, bundle_json)
}

fn handle_bundle_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<Bundle>(id) {
            Ok(Some(bundle)) => {
                return match serde_json::to_value(&bundle) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let bundles = stores.bundles.read().unwrap();
    match bundles.get(id) {
        Some(bundle) => match serde_json::to_value(bundle) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("bundle", id)),
    }
}

fn handle_bundle_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let wi_filter = req.params.get("work_item_id").and_then(|v| v.as_str());

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        let filters: Vec<Filter> = if let Some(wid) = wi_filter {
            vec![Filter {
                field: "work_item_id".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(wid.to_string()),
            }]
        } else {
            vec![]
        };
        match store.lock().unwrap().list::<Bundle>(&filters) {
            Ok(bundles) => {
                return match serde_json::to_value(&bundles) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let bundles = stores.bundles.read().unwrap();
    let bundle_list: Vec<&Bundle> = bundles
        .values()
        .filter(|b| wi_filter.is_none() || Some(b.work_item_id.as_str()) == wi_filter)
        .collect();

    match serde_json::to_value(&bundle_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_bundle_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: BundleStatus = match req.params.get("target_status") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(s) => s,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid target_status")),
        },
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("target_status is required")),
    };

    let role: Role = match req.params.get("role") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid role")),
        },
        None => Role::Coordinator,
    };

    let mut bundles = stores.bundles.write().unwrap();
    let bundle = match bundles.get_mut(&id) {
        Some(b) => b,
        None => return DaemonResponse::err(req.id, RpcError::not_found("bundle", &id)),
    };

    let from = bundle.status;
    let rules = bundle_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    bundle.status = target_status;
    bundle.updated_at = crate::id::now_millis();

    let bundle_json = match serde_json::to_value(&*bundle) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "bundle",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, bundle_json)
}

// --- Tick handlers ---

fn handle_tick_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let number = match req.params.get("number").and_then(|v| v.as_u64()) {
        Some(n) => n as u32,
        None => {
            return DaemonResponse::err(
                req.id,
                RpcError::invalid_params("number is required (positive integer)"),
            );
        }
    };

    let tick = Tick::new(number);
    let tick_json = match serde_json::to_value(&tick) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = tick.id.clone();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().create(tick.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    stores.ticks.write().unwrap().insert(id.clone(), tick);
    let _ = event_tx.send(DaemonEvent::record_created("tick", &id));

    DaemonResponse::ok(req.id, tick_json)
}

fn handle_tick_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<Tick>(id) {
            Ok(Some(tick)) => {
                return match serde_json::to_value(&tick) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let ticks = stores.ticks.read().unwrap();
    match ticks.get(id) {
        Some(tick) => match serde_json::to_value(tick) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("tick", id)),
    }
}

fn handle_tick_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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
        match store.lock().unwrap().list::<Tick>(&filters) {
            Ok(ticks) => {
                return match serde_json::to_value(&ticks) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let ticks = stores.ticks.read().unwrap();
    let tick_list: Vec<&Tick> = ticks
        .values()
        .filter(|t| status_filter.is_none() || Some(t.status) == status_filter)
        .collect();

    match serde_json::to_value(&tick_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_tick_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let target_status: TickStatus = match req.params.get("target_status") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(s) => s,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid target_status")),
        },
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("target_status is required")),
    };

    let role: Role = match req.params.get("role") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(_) => return DaemonResponse::err(req.id, RpcError::invalid_params("invalid role")),
        },
        None => Role::Integrator,
    };

    let mut ticks = stores.ticks.write().unwrap();
    let tick = match ticks.get_mut(&id) {
        Some(t) => t,
        None => return DaemonResponse::err(req.id, RpcError::not_found("tick", &id)),
    };

    let from = tick.status;
    let rules = tick_transitions();
    if let Err(e) = validate_transition(from, target_status, role, &rules) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
    }

    tick.status = target_status;
    tick.updated_at = crate::id::now_millis();

    let tick_json = match serde_json::to_value(&*tick) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::transition_completed(
        "tick",
        &id,
        &from.to_string(),
        &target_status.to_string(),
        &role.to_string(),
    ));

    DaemonResponse::ok(req.id, tick_json)
}

// --- Learning handlers ---

fn handle_learning_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
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
                return DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("invalid scope (workitem|phase|spec|plan|global)"),
                );
            }
        },
        None => {
            return DaemonResponse::err(req.id, RpcError::invalid_params("scope is required"));
        }
    };
    let content = req
        .params
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if source_id.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("source_id is required"));
    }
    if content.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("content is required"));
    }

    let learning = Learning::new(source_id, scope, content);
    let learning_json = match serde_json::to_value(&learning) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = learning.id.clone();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().create(learning.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    stores.learnings.write().unwrap().insert(id.clone(), learning);
    let _ = event_tx.send(DaemonEvent::record_created("learning", &id));

    DaemonResponse::ok(req.id, learning_json)
}

fn handle_learning_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<Learning>(id) {
            Ok(Some(learning)) => {
                return match serde_json::to_value(&learning) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let learnings = stores.learnings.read().unwrap();
    match learnings.get(id) {
        Some(learning) => match serde_json::to_value(learning) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("learning", id)),
    }
}

fn handle_learning_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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
        match store.lock().unwrap().list::<Learning>(&filters) {
            Ok(learnings) => {
                return match serde_json::to_value(&learnings) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let learnings = stores.learnings.read().unwrap();
    let learning_list: Vec<&Learning> = learnings
        .values()
        .filter(|l| scope_filter.is_none() || Some(l.scope) == scope_filter)
        .filter(|l| source_id_filter.is_none() || Some(l.source_id.as_str()) == source_id_filter.as_deref())
        .collect();

    match serde_json::to_value(&learning_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_learning_reinforce(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let mut learnings = stores.learnings.write().unwrap();
    let learning = match learnings.get_mut(&id) {
        Some(l) => l,
        None => return DaemonResponse::err(req.id, RpcError::not_found("learning", &id)),
    };

    learning.reinforce();
    learning.updated_at = crate::id::now_millis();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(learning.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let learning_json = match serde_json::to_value(&*learning) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::record_updated("learning", &id));

    DaemonResponse::ok(req.id, learning_json)
}

fn handle_learning_contradict(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let mut learnings = stores.learnings.write().unwrap();
    let learning = match learnings.get_mut(&id) {
        Some(l) => l,
        None => return DaemonResponse::err(req.id, RpcError::not_found("learning", &id)),
    };

    learning.contradict();
    learning.updated_at = crate::id::now_millis();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(learning.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let learning_json = match serde_json::to_value(&*learning) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::record_updated("learning", &id));

    DaemonResponse::ok(req.id, learning_json)
}

fn handle_learning_promote(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let mut learnings = stores.learnings.write().unwrap();
    let learning = match learnings.get_mut(&id) {
        Some(l) => l,
        None => return DaemonResponse::err(req.id, RpcError::not_found("learning", &id)),
    };

    learning.promote();
    learning.updated_at = crate::id::now_millis();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(learning.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let learning_json = match serde_json::to_value(&*learning) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::record_updated("learning", &id));

    DaemonResponse::ok(req.id, learning_json)
}

fn handle_learning_demote(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let mut learnings = stores.learnings.write().unwrap();
    let learning = match learnings.get_mut(&id) {
        Some(l) => l,
        None => return DaemonResponse::err(req.id, RpcError::not_found("learning", &id)),
    };

    learning.demote();
    learning.updated_at = crate::id::now_millis();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(learning.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let learning_json = match serde_json::to_value(&*learning) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::record_updated("learning", &id));

    DaemonResponse::ok(req.id, learning_json)
}

// --- Lock handlers ---

fn handle_lock_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let resource = req
        .params
        .get("resource")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let holder_id = req
        .params
        .get("holder_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let granted_by = req
        .params
        .get("granted_by")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if resource.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("resource is required"));
    }
    if holder_id.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("holder_id is required"));
    }
    if granted_by.is_empty() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("granted_by is required"));
    }

    let lock = Lock::new(resource, holder_id, granted_by);
    let lock_json = match serde_json::to_value(&lock) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = lock.id.clone();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().create(lock.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    stores.locks.write().unwrap().insert(id.clone(), lock);
    let _ = event_tx.send(DaemonEvent::record_created("lock", &id));

    DaemonResponse::ok(req.id, lock_json)
}

fn handle_lock_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<Lock>(id) {
            Ok(Some(lock)) => {
                return match serde_json::to_value(&lock) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let locks = stores.locks.read().unwrap();
    match locks.get(id) {
        Some(lock) => match serde_json::to_value(lock) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("lock", id)),
    }
}

fn handle_lock_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    // Optionally filter by resource
    let resource_filter = req
        .params
        .get("resource")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Optionally filter by holder_id
    let holder_filter = req
        .params
        .get("holder_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Optionally filter by active-only
    let active_only = req.params.get("active_only").and_then(|v| v.as_bool()).unwrap_or(false);

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        let mut filters: Vec<Filter> = vec![];
        if let Some(resource) = &resource_filter {
            filters.push(Filter {
                field: "resource".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(resource.clone()),
            });
        }
        if let Some(holder_id) = &holder_filter {
            filters.push(Filter {
                field: "holder_id".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(holder_id.clone()),
            });
        }
        if active_only {
            filters.push(Filter {
                field: "status".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String("Active".to_string()),
            });
        }
        match store.lock().unwrap().list::<Lock>(&filters) {
            Ok(locks) => {
                return match serde_json::to_value(&locks) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let locks = stores.locks.read().unwrap();
    let lock_list: Vec<&Lock> = locks
        .values()
        .filter(|l| resource_filter.is_none() || Some(l.resource.as_str()) == resource_filter.as_deref())
        .filter(|l| holder_filter.is_none() || Some(l.holder_id.as_str()) == holder_filter.as_deref())
        .filter(|l| !active_only || l.is_active())
        .collect();

    match serde_json::to_value(&lock_list) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_lock_release(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let mut locks = stores.locks.write().unwrap();
    let lock = match locks.get_mut(&id) {
        Some(l) => l,
        None => return DaemonResponse::err(req.id, RpcError::not_found("lock", &id)),
    };

    if !lock.is_active() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("lock is not active"));
    }

    lock.release();
    lock.updated_at = crate::id::now_millis();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(lock.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let lock_json = match serde_json::to_value(&*lock) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::record_updated("lock", &id));

    DaemonResponse::ok(req.id, lock_json)
}

fn handle_lock_expire(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let mut locks = stores.locks.write().unwrap();
    let lock = match locks.get_mut(&id) {
        Some(l) => l,
        None => return DaemonResponse::err(req.id, RpcError::not_found("lock", &id)),
    };

    if !lock.is_active() {
        return DaemonResponse::err(req.id, RpcError::invalid_params("lock is not active"));
    }

    lock.expire();
    lock.updated_at = crate::id::now_millis();

    // Persist to TaskStore if available
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(lock.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let lock_json = match serde_json::to_value(&*lock) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::record_updated("lock", &id));

    DaemonResponse::ok(req.id, lock_json)
}

// --- Worktree handlers ---

fn handle_worktree_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    req: DaemonRequest,
) -> DaemonResponse {
    let work_item_id = match req.params.get("work_item_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("work_item_id is required")),
    };

    let base_ref = req
        .params
        .get("base_ref")
        .and_then(|v| v.as_str())
        .unwrap_or("HEAD")
        .to_string();

    // Validate the work item exists (TaskStore first, fallback to HashMap)
    {
        let found = if let Some(store) = &stores.store {
            store.lock().unwrap().get::<WorkItem>(&work_item_id).ok().is_some()
        } else {
            false
        };
        if !found {
            let work_items = stores.work_items.read().unwrap();
            if !work_items.contains_key(&work_item_id) {
                return DaemonResponse::err(req.id, RpcError::not_found("work_item", &work_item_id));
            }
        }
    }

    // Check if worktree already exists before attempting git operations
    if worktree_mgr.exists(&work_item_id) {
        return DaemonResponse::err(
            req.id,
            RpcError::invalid_params(&format!("worktree already exists for work item {work_item_id}")),
        );
    }

    match worktree_mgr.create(&work_item_id, &base_ref) {
        Ok(path) => {
            let _ = event_tx.send(DaemonEvent::new(
                "worktree.created",
                json!({ "work_item_id": work_item_id, "path": path.to_string_lossy() }),
            ));
            DaemonResponse::ok(
                req.id,
                json!({ "work_item_id": work_item_id, "path": path.to_string_lossy() }),
            )
        }
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_worktree_list(worktree_mgr: &WorktreeManager, req: DaemonRequest) -> DaemonResponse {
    match worktree_mgr.list() {
        Ok(worktrees) => match serde_json::to_value(&worktrees) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_worktree_cleanup(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    req: DaemonRequest,
) -> DaemonResponse {
    let work_item_id = match req.params.get("work_item_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("work_item_id is required")),
    };

    // Validate the work item exists (TaskStore first, fallback to HashMap)
    {
        let found = if let Some(store) = &stores.store {
            store.lock().unwrap().get::<WorkItem>(&work_item_id).ok().is_some()
        } else {
            false
        };
        if !found {
            let work_items = stores.work_items.read().unwrap();
            if !work_items.contains_key(&work_item_id) {
                return DaemonResponse::err(req.id, RpcError::not_found("work_item", &work_item_id));
            }
        }
    }

    let path = worktree_mgr.worktree_path(&work_item_id);
    match worktree_mgr.cleanup(&work_item_id) {
        Ok(()) => {
            let _ = event_tx.send(DaemonEvent::new(
                "worktree.cleaned",
                json!({ "work_item_id": work_item_id, "path": path.to_string_lossy() }),
            ));
            DaemonResponse::ok(
                req.id,
                json!({ "work_item_id": work_item_id, "path": path.to_string_lossy(), "status": "cleaned" }),
            )
        }
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_worktree_refresh(worktree_mgr: &WorktreeManager, req: DaemonRequest) -> DaemonResponse {
    let work_item_id = match req.params.get("work_item_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("work_item_id is required")),
    };

    let new_base_ref = req
        .params
        .get("new_base_ref")
        .and_then(|v| v.as_str())
        .unwrap_or("HEAD")
        .to_string();

    match worktree_mgr.refresh(&work_item_id, &new_base_ref) {
        Ok(()) => DaemonResponse::ok(req.id, json!({ "work_item_id": work_item_id, "status": "refreshed" })),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

// --- Integrator handlers ---

/// Run validation commands against the repo, returning (success, combined_log).
fn run_validation_commands(commands: &[String]) -> (bool, String) {
    let mut log = String::new();
    for cmd in commands {
        log.push_str(&format!("=== Running: {cmd} ===\n"));
        let output = Command::new("sh").arg("-c").arg(cmd).output();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stdout.is_empty() {
                    log.push_str(&stdout);
                    if !stdout.ends_with('\n') {
                        log.push('\n');
                    }
                }
                if !stderr.is_empty() {
                    log.push_str(&stderr);
                    if !stderr.ends_with('\n') {
                        log.push('\n');
                    }
                }
                if !out.status.success() {
                    log.push_str(&format!("=== FAILED (exit code {:?}) ===\n", out.status.code()));
                    return (false, log);
                }
                log.push_str("=== PASSED ===\n");
            }
            Err(e) => {
                log.push_str(&format!("=== FAILED to execute: {e} ===\n"));
                return (false, log);
            }
        }
    }
    (true, log)
}

/// Get the current git HEAD SHA.
fn get_git_head_sha() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn handle_integrator_validate(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    let tick_id = match req.params.get("tick_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("tick_id is required")),
    };

    // Verify tick exists and is in Sealing state
    {
        let ticks = stores.ticks.read().unwrap();
        let tick = match ticks.get(&tick_id) {
            Some(t) => t,
            None => return DaemonResponse::err(req.id, RpcError::not_found("tick", &tick_id)),
        };
        if tick.status != TickStatus::Sealing {
            return DaemonResponse::err(
                req.id,
                RpcError::transition_rejected(&format!(
                    "tick must be in Sealing state to validate (currently {:?})",
                    tick.status
                )),
            );
        }
    }

    // Transition to Validating
    {
        let mut ticks = stores.ticks.write().unwrap();
        let tick = ticks.get_mut(&tick_id).unwrap();
        tick.status = TickStatus::Validating;
        tick.updated_at = crate::id::now_millis();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store.lock().unwrap().update(tick.clone())
        {
            return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
        }
    }
    let _ = event_tx.send(DaemonEvent::transition_completed(
        "tick",
        &tick_id,
        "Sealing",
        "Validating",
        "Integrator",
    ));

    // Run validation commands
    let (all_passed, validation_log) = run_validation_commands(&integrator_config.validation_commands);

    // Transition to Published or Failed based on results
    let final_status = if all_passed { TickStatus::Published } else { TickStatus::Failed };

    let tick_json = {
        let mut ticks = stores.ticks.write().unwrap();
        let tick = ticks.get_mut(&tick_id).unwrap();
        tick.status = final_status;
        tick.validation_log = validation_log;
        tick.updated_at = crate::id::now_millis();

        if all_passed {
            tick.integration_sha = get_git_head_sha();
        }

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store.lock().unwrap().update(tick.clone())
        {
            return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
        }

        match serde_json::to_value(&*tick) {
            Ok(v) => v,
            Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        }
    };

    if all_passed {
        let sha = tick_json
            .get("integration_sha")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let _ = event_tx.send(DaemonEvent::tick_published(&tick_id, sha));
    } else {
        let _ = event_tx.send(DaemonEvent::tick_validation_failed(
            &tick_id,
            "validation commands failed",
        ));
    }

    DaemonResponse::ok(req.id, tick_json)
}

fn handle_integrator_publish(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    let tick_id = match req.params.get("tick_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("tick_id is required")),
    };

    // Verify tick exists and determine current state
    let current_status = {
        let ticks = stores.ticks.read().unwrap();
        match ticks.get(&tick_id) {
            Some(t) => t.status,
            None => return DaemonResponse::err(req.id, RpcError::not_found("tick", &tick_id)),
        }
    };

    // If Open, transition to Sealing first
    if current_status == TickStatus::Open {
        let mut ticks = stores.ticks.write().unwrap();
        let tick = ticks.get_mut(&tick_id).unwrap();
        tick.status = TickStatus::Sealing;
        tick.updated_at = crate::id::now_millis();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store.lock().unwrap().update(tick.clone())
        {
            return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
        }

        let _ = event_tx.send(DaemonEvent::transition_completed(
            "tick",
            &tick_id,
            "Open",
            "Sealing",
            "Integrator",
        ));
    } else if current_status != TickStatus::Sealing {
        return DaemonResponse::err(
            req.id,
            RpcError::transition_rejected(&format!(
                "integrator.publish requires tick in Open or Sealing state (currently {:?})",
                current_status
            )),
        );
    }

    // Now delegate to validate (tick is in Sealing state)
    let validate_req = DaemonRequest::new(req.id, "integrator.validate", json!({ "tick_id": tick_id }));
    handle_integrator_validate(stores, event_tx, integrator_config, validate_req)
}

/// Find the latest Published Tick (by highest tick number).
fn find_latest_published_tick(stores: &Arc<Stores>) -> Option<Tick> {
    let ticks = stores.ticks.read().unwrap();
    ticks
        .values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .cloned()
}

// --- Validator handlers ---

fn handle_validator_validate(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let validator = match &stores.validator {
        Some(v) => v.clone(),
        None => {
            return DaemonResponse::err(req.id, RpcError::internal("validator is not enabled"));
        }
    };

    let collection = match req.params.get("collection").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => {
            return DaemonResponse::err(req.id, RpcError::invalid_params("collection is required"));
        }
    };

    let target_id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            return DaemonResponse::err(req.id, RpcError::invalid_params("id is required"));
        }
    };

    let report = match collection.as_str() {
        "plan" | "plans" => {
            let plans = stores.plans.read().unwrap();
            let plan = match plans.get(&target_id) {
                Some(p) => p.clone(),
                None => {
                    return DaemonResponse::err(req.id, RpcError::not_found("plan", &target_id));
                }
            };
            drop(plans);
            validator.validate_plan(&target_id, &plan.title, &plan.description, &plan.acceptance_criteria)
        }
        "spec" | "specs" => {
            let specs = stores.specs.read().unwrap();
            let spec = match specs.get(&target_id) {
                Some(s) => s.clone(),
                None => {
                    return DaemonResponse::err(req.id, RpcError::not_found("spec", &target_id));
                }
            };
            drop(specs);
            // Get parent plan title for context
            let plan_title = stores
                .plans
                .read()
                .unwrap()
                .get(&spec.plan_id)
                .map(|p| p.title.clone())
                .unwrap_or_default();
            validator.validate_spec(&target_id, &spec.title, &spec.description, &plan_title)
        }
        "phase" | "phases" => {
            let phases = stores.phases.read().unwrap();
            let phase = match phases.get(&target_id) {
                Some(p) => p.clone(),
                None => {
                    return DaemonResponse::err(req.id, RpcError::not_found("phase", &target_id));
                }
            };
            drop(phases);
            // Get parent spec title for context
            let spec_title = stores
                .specs
                .read()
                .unwrap()
                .get(&phase.spec_id)
                .map(|s| s.title.clone())
                .unwrap_or_default();
            validator.validate_phase(&target_id, &phase.title, &phase.description, phase.order, &spec_title)
        }
        _ => {
            return DaemonResponse::err(
                req.id,
                RpcError::invalid_params(&format!("unsupported collection for validation: {}", collection)),
            );
        }
    };

    match report {
        Ok(report) => {
            // Persist to TaskStore
            if let Some(store) = &stores.store
                && let Err(e) = store.lock().unwrap().create(report.clone())
            {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
            DaemonResponse::ok(req.id, serde_json::to_value(&report).unwrap())
        }
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

fn handle_validator_report(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let report_id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            return DaemonResponse::err(req.id, RpcError::invalid_params("id is required"));
        }
    };

    // Read from TaskStore
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<ValidationReport>(&report_id) {
            Ok(Some(report)) => {
                return DaemonResponse::ok(req.id, serde_json::to_value(&report).unwrap());
            }
            Ok(None) => {
                return DaemonResponse::err(req.id, RpcError::not_found("validation_report", &report_id));
            }
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    DaemonResponse::err(req.id, RpcError::internal("TaskStore not available"))
}

fn handle_validator_reports(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    if let Some(store) = &stores.store {
        let mut filters = vec![];

        if let Some(target_id) = req.params.get("target_id").and_then(|v| v.as_str()) {
            filters.push(Filter {
                field: "target_id".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(target_id.to_string()),
            });
        }

        if let Some(target_collection) = req.params.get("target_collection").and_then(|v| v.as_str()) {
            filters.push(Filter {
                field: "target_collection".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(target_collection.to_string()),
            });
        }

        match store.lock().unwrap().list::<ValidationReport>(&filters) {
            Ok(reports) => DaemonResponse::ok(req.id, serde_json::to_value(&reports).unwrap()),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        }
    } else {
        DaemonResponse::ok(req.id, json!([]))
    }
}

// --- Tool handlers ---

fn handle_tool_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let tool_runner = &stores.tool_runner;
    let names = tool_runner.available_tools();
    let tools: Vec<serde_json::Value> = names
        .iter()
        .filter_map(|name| {
            tool_runner.get_tool(name).map(|entry| {
                json!({
                    "name": entry.name,
                    "command": entry.command,
                    "timeout_secs": entry.timeout_secs,
                    "worktree": entry.worktree,
                })
            })
        })
        .collect();
    DaemonResponse::ok(req.id, json!({ "tools": tools }))
}

// --- Agent handlers ---

fn handle_agent_start(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    req: DaemonRequest,
) -> DaemonResponse {
    let agent_type: AgentType = match req.params.get("agent_type") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(t) => t,
            Err(_) => {
                return DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("invalid agent_type (implementer|reviewer)"),
                );
            }
        },
        None => {
            return DaemonResponse::err(req.id, RpcError::invalid_params("agent_type is required"));
        }
    };

    let work_item_id = req
        .params
        .get("work_item_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let bundle_id = req
        .params
        .get("bundle_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Validate: Implementer needs work_item_id, Reviewer needs bundle_id
    match agent_type {
        AgentType::Implementer => {
            if work_item_id.is_none() {
                return DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("work_item_id is required for implementer agents"),
                );
            }
        }
        AgentType::Reviewer => {
            if bundle_id.is_none() {
                return DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("bundle_id is required for reviewer agents"),
                );
            }
        }
    }

    // Create agent session with model from config (placeholder — will be wired up in Phase 2)
    let mut session = AgentSession::new(agent_type, "claude-sonnet-4-6".to_string());
    session.work_item_id = work_item_id;
    session.bundle_id = bundle_id;

    let session_json = match serde_json::to_value(&session) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let id = session.id.clone();

    // Persist to TaskStore
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().create(session.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    stores.agent_sessions.write().unwrap().insert(id.clone(), session);
    let _ = event_tx.send(DaemonEvent::record_created("agent_session", &id));
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&id, AgentStatus::Starting));

    // Spawn agent task as a Tokio background task
    let task_stores = stores.clone();
    let task_event_tx = event_tx.clone();
    let task_worktree_mgr = worktree_mgr.clone();
    let task_id = id.clone();
    tokio::spawn(async move {
        crate::agents::executor::run_agent_task(task_id, agent_type, task_stores, task_event_tx, task_worktree_mgr)
            .await;
    });

    DaemonResponse::ok(req.id, session_json)
}

fn handle_agent_stop(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("session_id is required")),
    };

    let mut sessions = stores.agent_sessions.write().unwrap();
    let session = match sessions.get_mut(session_id) {
        Some(s) => s,
        None => return DaemonResponse::err(req.id, RpcError::not_found("agent_session", session_id)),
    };

    if session.status.is_terminal() {
        return DaemonResponse::err(
            req.id,
            RpcError::transition_rejected(&format!("agent is already in terminal state: {}", session.status)),
        );
    }

    if let Err(e) = session.transition_to(AgentStatus::Cancelled) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e));
    }

    // Persist to TaskStore
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(session.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let session_json = match serde_json::to_value(&*session) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::record_updated("agent_session", session_id));
    let _ = event_tx.send(DaemonEvent::agent_status_changed(session_id, AgentStatus::Cancelled));
    DaemonResponse::ok(req.id, session_json)
}

fn handle_agent_pause(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("session_id is required")),
    };

    let mut sessions = stores.agent_sessions.write().unwrap();
    let session = match sessions.get_mut(session_id) {
        Some(s) => s,
        None => return DaemonResponse::err(req.id, RpcError::not_found("agent_session", session_id)),
    };

    if session.status.is_terminal() {
        return DaemonResponse::err(
            req.id,
            RpcError::transition_rejected(&format!("agent is already in terminal state: {}", session.status)),
        );
    }

    if let Err(e) = session.transition_to(AgentStatus::Paused) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e));
    }

    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(session.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let session_json = match serde_json::to_value(&*session) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::record_updated("agent_session", session_id));
    let _ = event_tx.send(DaemonEvent::agent_status_changed(session_id, AgentStatus::Paused));
    DaemonResponse::ok(req.id, session_json)
}

fn handle_agent_resume(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("session_id is required")),
    };

    let mut sessions = stores.agent_sessions.write().unwrap();
    let session = match sessions.get_mut(session_id) {
        Some(s) => s,
        None => return DaemonResponse::err(req.id, RpcError::not_found("agent_session", session_id)),
    };

    if let Err(e) = session.transition_to(AgentStatus::Running) {
        return DaemonResponse::err(req.id, RpcError::transition_rejected(&e));
    }

    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(session.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let session_json = match serde_json::to_value(&*session) {
        Ok(v) => v,
        Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    };

    let _ = event_tx.send(DaemonEvent::record_updated("agent_session", session_id));
    let _ = event_tx.send(DaemonEvent::agent_status_changed(session_id, AgentStatus::Running));
    DaemonResponse::ok(req.id, session_json)
}

fn handle_agent_status(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("session_id is required")),
    };

    // Try TaskStore first, fall back to HashMap
    if let Some(store) = &stores.store {
        match store.lock().unwrap().get::<AgentSession>(session_id) {
            Ok(Some(session)) => {
                return match serde_json::to_value(&session) {
                    Ok(v) => DaemonResponse::ok(req.id, v),
                    Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
                };
            }
            Ok(None) => {}
            Err(e) => {
                return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
            }
        }
    }

    let sessions = stores.agent_sessions.read().unwrap();
    match sessions.get(session_id) {
        Some(session) => match serde_json::to_value(session) {
            Ok(v) => DaemonResponse::ok(req.id, v),
            Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        },
        None => DaemonResponse::err(req.id, RpcError::not_found("agent_session", session_id)),
    }
}

fn handle_agent_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let status_filter: Option<AgentStatus> = req
        .params
        .get("status")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let type_filter: Option<AgentType> = req
        .params
        .get("agent_type")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    if let Some(store) = &stores.store {
        let mut filters: Vec<Filter> = vec![];
        if let Some(status) = &status_filter {
            filters.push(Filter {
                field: "status".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(status.to_string()),
            });
        }
        if let Some(agent_type) = &type_filter {
            filters.push(Filter {
                field: "agent_type".to_string(),
                op: FilterOp::Eq,
                value: IndexValue::String(agent_type.to_string()),
            });
        }
        match store.lock().unwrap().list::<AgentSession>(&filters) {
            Ok(sessions) => match serde_json::to_value(&sessions) {
                Ok(v) => return DaemonResponse::ok(req.id, v),
                Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
            },
            Err(e) => return DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
        }
    }

    // Fallback to HashMap
    let sessions = stores.agent_sessions.read().unwrap();
    let mut result: Vec<&AgentSession> = sessions.values().collect();

    if let Some(status) = status_filter {
        result.retain(|s| s.status == status);
    }
    if let Some(agent_type) = type_filter {
        result.retain(|s| s.agent_type == agent_type);
    }

    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(req.id, v),
        Err(e) => DaemonResponse::err(req.id, RpcError::internal(&e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    fn test_stores_with_taskstore() -> Arc<Stores> {
        let id = crate::id::generate_id();
        let dir = std::env::temp_dir().join(format!("loopr-handler-test-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        // Initialize a git repo so install_git_hooks works
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
        let mut store = taskstore::Store::open(&dir).unwrap();
        store.rebuild_indexes::<Plan>().unwrap();
        store.rebuild_indexes::<Spec>().unwrap();
        store.rebuild_indexes::<Phase>().unwrap();
        store.rebuild_indexes::<WorkItem>().unwrap();
        store.rebuild_indexes::<Bundle>().unwrap();
        store.rebuild_indexes::<Tick>().unwrap();
        store.rebuild_indexes::<Learning>().unwrap();
        store.rebuild_indexes::<Lock>().unwrap();
        store.rebuild_indexes::<ValidationReport>().unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        Arc::new(stores)
    }

    /// Creates stores with TaskStore AND a validator (DocValidator placeholder via Arc).
    /// This activates the validation gate for Draft → Active transitions.
    fn test_stores_with_validator() -> Arc<Stores> {
        let id = crate::id::generate_id();
        let dir = std::env::temp_dir().join(format!("loopr-handler-test-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
        let mut store = taskstore::Store::open(&dir).unwrap();
        store.rebuild_indexes::<Plan>().unwrap();
        store.rebuild_indexes::<Spec>().unwrap();
        store.rebuild_indexes::<Phase>().unwrap();
        store.rebuild_indexes::<WorkItem>().unwrap();
        store.rebuild_indexes::<Bundle>().unwrap();
        store.rebuild_indexes::<Tick>().unwrap();
        store.rebuild_indexes::<Learning>().unwrap();
        store.rebuild_indexes::<Lock>().unwrap();
        store.rebuild_indexes::<ValidationReport>().unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        // Create a DocValidator to enable the validation gate.
        // Tests don't call the LLM — they only check that the gate logic works.
        let validator_config = crate::config::ValidatorConfig {
            enabled: true,
            api_key_env: "NONEXISTENT_TEST_KEY".to_string(),
            ..crate::config::ValidatorConfig::default()
        };
        stores.validator = Some(Arc::new(crate::validator::DocValidator::new(validator_config)));
        Arc::new(stores)
    }

    fn test_event_tx() -> broadcast::Sender<DaemonEvent> {
        let (tx, _) = broadcast::channel(16);
        tx
    }

    fn test_worktree_mgr() -> WorktreeManager {
        WorktreeManager::new(
            PathBuf::from("/nonexistent/repo"),
            PathBuf::from("/nonexistent/worktrees"),
        )
    }

    fn test_integrator_config() -> IntegratorConfig {
        IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        }
    }

    // --- dispatch tests ---

    #[test]
    fn test_dispatch_unknown_method() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "unknown.method", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unknown.method"));
    }

    #[test]
    fn test_dispatch_handshake() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.handshake", json!({"client_version": "0.1.0"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["protocol"], "ndjson/1");
    }

    #[test]
    fn test_dispatch_status_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["version"].is_string());
        assert!(result["pid"].is_number());
        assert_eq!(result["counts"]["plans"], 0);
        assert_eq!(result["counts"]["work_items"], 0);
        // Without TaskStore, reports disabled
        assert_eq!(result["taskstore"]["enabled"], false);
    }

    #[test]
    fn test_dispatch_status_with_records() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Insert a plan
        let plan = Plan::new("Test".into(), "".into(), "".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);
        // Insert a work item
        let wi = WorkItem::new("p-1".into(), "WI".into(), "".into());
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["counts"]["plans"], 1);
        assert_eq!(result["counts"]["work_items"], 1);
        assert_eq!(result["counts"]["specs"], 0);
    }

    #[test]
    fn test_dispatch_status_with_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan via TaskStore directly
        let plan = Plan::new("TS Plan".into(), "".into(), "".into());
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create(plan.clone())
            .unwrap();

        // Create a spec via TaskStore directly
        let spec = Spec::new(plan.id.clone(), "TS Spec".into(), "".into());
        stores.store.as_ref().unwrap().lock().unwrap().create(spec).unwrap();

        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();

        // TaskStore should be enabled and show counts
        assert_eq!(result["taskstore"]["enabled"], true);
        assert_eq!(result["taskstore"]["counts"]["plans"], 1);
        assert_eq!(result["taskstore"]["counts"]["specs"], 1);
        assert_eq!(result["taskstore"]["counts"]["phases"], 0);
        assert_eq!(result["taskstore"]["counts"]["work_items"], 0);

        // HashMap counts should be 0 (we only wrote to TaskStore)
        assert_eq!(result["counts"]["plans"], 0);
    }

    #[test]
    fn test_dispatch_shutdown() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.shutdown", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "shutting_down");
        // Verify event was broadcast
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "system.shutdown");
    }

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
        let stores = test_stores_with_taskstore();
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
        let stores = test_stores_with_taskstore();
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
        // Create two plans
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"})),
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
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create two plans (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"})),
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

        // Transition Draft → Active
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

        // Try to skip Draft → Complete (invalid: must go through Active)
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

        // No role specified — defaults to Coordinator, which is valid for hierarchy transitions
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
        let stores = test_stores_with_taskstore();
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

        // Transition Draft → Active
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
        assert_eq!(plan.status, PlanStatus::Active);
    }

    // --- spec.create tests ---

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

    #[test]
    fn test_spec_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({
                "plan_id": plan_id,
                "title": "Test Spec",
                "description": "A spec"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Spec");
        assert_eq!(result["plan_id"], plan_id);
        assert_eq!(result["status"], "draft");
        assert_eq!(stores.specs.read().unwrap().len(), 1);
    }

    #[test]
    fn test_spec_create_missing_plan_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.create", json!({"title": "Spec"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("plan_id"));
    }

    #[test]
    fn test_spec_create_plan_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.create", json!({"plan_id": "nonexistent", "title": "Spec"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_spec_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        let req = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "description": "no title"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_spec_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        let _ = rx.try_recv(); // consume plan create event

        let req = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "spec");
    }

    #[test]
    fn test_spec_create_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Persisted Spec", "description": "desc"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let spec_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Spec> = store.get(&spec_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted Spec");
    }

    // --- spec.get tests ---

    #[test]
    fn test_spec_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "My Spec"})),
        );
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "spec.get", json!({"id": spec_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My Spec");
    }

    #[test]
    fn test_spec_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_spec_get_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        // Create a spec (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "TaskStore Spec"}));
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        assert!(!create_resp.is_error());
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.specs.write().unwrap().remove(&spec_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(3, "spec.get", json!({"id": spec_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore Spec");
    }

    // --- spec.list tests ---

    #[test]
    fn test_spec_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_spec_list_filtered_by_plan_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id_1 = create_test_plan(&stores, &tx, &wm);

        // Create a second plan
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "plan.create", json!({"title": "Plan 2"})),
        );
        let plan_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create specs under different plans
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id_1, "title": "Spec A"})),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "spec.create", json!({"plan_id": plan_id_2, "title": "Spec B"})),
        );

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "spec.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by plan_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(5, "spec.list", json!({"plan_id": plan_id_1})),
        );
        let specs = filtered_resp.result.unwrap();
        let arr = specs.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Spec A");
    }

    #[test]
    fn test_spec_list_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan first
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan X"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create two specs under that plan (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec A"})),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "spec.create", json!({"plan_id": plan_id, "title": "Spec B"})),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.specs.write().unwrap().clear();

        // List should still return both specs via TaskStore
        let req = DaemonRequest::new(4, "spec.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let specs = resp.result.unwrap();
        assert_eq!(specs.as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(5, "spec.list", json!({"plan_id": plan_id}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_specs = filtered_resp.result.unwrap();
        assert_eq!(filtered_specs.as_array().unwrap().len(), 2);
    }

    // --- spec.transition tests ---

    #[test]
    fn test_spec_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        let _ = rx.try_recv(); // consume plan create event

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"})),
        );
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "spec");
        assert_eq!(event.data["from"], "draft");
        assert_eq!(event.data["to"], "active");
    }

    #[test]
    fn test_spec_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"})),
        );
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_spec_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"})),
        );
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_spec_transition_not_found() {
        let stores = test_stores();
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_spec_transition_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        // Create spec (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "spec.create",
                json!({"plan_id": plan_id, "title": "Transition Spec"}),
            ),
        );
        assert!(!create_resp.is_error());
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft → Active
        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Spec> = store.get(&spec_id).unwrap();
        assert!(retrieved.is_some());
        let spec = retrieved.unwrap();
        assert_eq!(spec.status, SpecStatus::Active);
    }

    // --- phase handlers ---

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

    #[test]
    fn test_phase_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({
                "spec_id": spec_id,
                "title": "Test Phase",
                "description": "A phase",
                "order": 1
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Phase");
        assert_eq!(result["spec_id"], spec_id);
        assert_eq!(result["status"], "draft");
        assert_eq!(result["order"], 1);
        assert_eq!(stores.phases.read().unwrap().len(), 1);
    }

    #[test]
    fn test_phase_create_missing_spec_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.create", json!({"title": "Phase"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("spec_id"));
    }

    #[test]
    fn test_phase_create_spec_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.create", json!({"spec_id": "nonexistent", "title": "Phase"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_phase_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"spec_id": spec_id, "description": "no title"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_phase_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        // Drain plan+spec create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "phase");
    }

    #[test]
    fn test_phase_create_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Persisted Phase", "description": "desc", "order": 1}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let phase_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Phase> = store.get(&phase_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted Phase");
    }

    #[test]
    fn test_phase_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"spec_id": spec_id, "title": "My Phase", "order": 3}),
            ),
        );
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(21, "phase.get", json!({"id": phase_id})),
        );
        assert!(!get_resp.is_error());
        let result = get_resp.result.unwrap();
        assert_eq!(result["title"], "My Phase");
        assert_eq!(result["order"], 3);
    }

    #[test]
    fn test_phase_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_phase_get_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        // Create a phase (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"spec_id": spec_id, "title": "TaskStore Phase", "order": 1}),
        );
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        assert!(!create_resp.is_error());
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.phases.write().unwrap().remove(&phase_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(21, "phase.get", json!({"id": phase_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore Phase");
    }

    #[test]
    fn test_phase_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_phase_list_filtered_by_spec_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id_1) = create_test_spec(&stores, &tx, &wm);

        // Create a second spec under the same plan
        let plan_id = _plan_id;
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "spec.create", json!({"plan_id": plan_id, "title": "Spec 2"})),
        );
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
                json!({"spec_id": spec_id_1, "title": "Phase A", "order": 1}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": spec_id_2, "title": "Phase B", "order": 1}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "phase.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by spec_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "phase.list", json!({"spec_id": spec_id_1})),
        );
        let phases = filtered_resp.result.unwrap();
        let arr = phases.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Phase A");
    }

    #[test]
    fn test_phase_list_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id_1) = create_test_spec(&stores, &tx, &wm);

        // Create a second spec under the same plan
        let plan_id = _plan_id;
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "spec.create", json!({"plan_id": plan_id, "title": "Spec 2"})),
        );
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
                json!({"spec_id": spec_id_1, "title": "Phase A", "order": 1}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": spec_id_2, "title": "Phase B", "order": 1}),
            ),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.phases.write().unwrap().clear();

        // List all should still return both phases via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "phase.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(31, "phase.list", json!({"spec_id": spec_id_1}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_phases = filtered_resp.result.unwrap();
        let arr = filtered_phases.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Phase A");
    }

    #[test]
    fn test_phase_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        // Drain plan+spec create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"})),
        );
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "phase");
        assert_eq!(event.data["from"], "draft");
        assert_eq!(event.data["to"], "active");
    }

    #[test]
    fn test_phase_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"})),
        );
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_phase_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"})),
        );
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_phase_transition_not_found() {
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_phase_transition_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm);

        // Create phase (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Transition Phase"}),
            ),
        );
        assert!(!create_resp.is_error());
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft → Active
        let req = DaemonRequest::new(
            3,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Phase> = store.get(&phase_id).unwrap();
        assert!(retrieved.is_some());
        let phase = retrieved.unwrap();
        assert_eq!(phase.status, PhaseStatus::Active);
    }

    // --- work_item handlers ---

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

    #[test]
    fn test_work_item_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            30,
            "work_item.create",
            json!({
                "phase_id": phase_id,
                "title": "Implement auth",
                "description": "Add JWT signing"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Implement auth");
        assert_eq!(result["phase_id"], phase_id);
        assert_eq!(result["status"], "Draft");
        assert_eq!(stores.work_items.read().unwrap().len(), 1);
    }

    #[test]
    fn test_work_item_create_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            30,
            "work_item.create",
            json!({"phase_id": phase_id, "title": "Persisted WI", "description": "desc"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<WorkItem> = store.get(&wi_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted WI");
    }

    #[test]
    fn test_work_item_create_missing_phase_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "work_item.create", json!({"title": "WI"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("phase_id"));
    }

    #[test]
    fn test_work_item_create_phase_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "work_item.create", json!({"phase_id": "nonexistent", "title": "WI"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_item_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            30,
            "work_item.create",
            json!({"phase_id": phase_id, "description": "no title"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_work_item_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
        // Drain plan+spec+phase create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "WI"}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "work_item");
    }

    #[test]
    fn test_work_item_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "My WI"})),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "work_item.get", json!({"id": wi_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My WI");
    }

    #[test]
    fn test_work_item_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "work_item.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_item_get_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Create a work item (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(
            30,
            "work_item.create",
            json!({"phase_id": phase_id, "title": "TaskStore WI"}),
        );
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        assert!(!create_resp.is_error());
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.work_items.write().unwrap().remove(&wi_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(31, "work_item.get", json!({"id": wi_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore WI");
    }

    #[test]
    fn test_work_item_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "work_item.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_work_item_list_filtered_by_phase_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id, phase_id_1) = create_test_phase(&stores, &tx, &wm);

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
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id_1, "title": "WI A"})),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "work_item.create", json!({"phase_id": phase_id_2, "title": "WI B"})),
        );

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(40, "work_item.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by phase_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(41, "work_item.list", json!({"phase_id": phase_id_1})),
        );
        let items = filtered_resp.result.unwrap();
        let arr = items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "WI A");
    }

    #[test]
    fn test_work_item_list_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id_1) = create_test_phase(&stores, &tx, &wm);

        // Create a second phase under the same spec
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": _spec_id, "title": "Phase 2", "order": 2}),
            ),
        );
        let phase_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create work items under different phases (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id_1, "title": "WI A"})),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "work_item.create", json!({"phase_id": phase_id_2, "title": "WI B"})),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.work_items.write().unwrap().clear();

        // List all should still return both work items via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(40, "work_item.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(41, "work_item.list", json!({"phase_id": phase_id_1}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        let arr = filtered_items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "WI A");
    }

    #[test]
    fn test_work_item_transition_draft_to_ready() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
        // Drain plan+spec+phase create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "WI"})),
        );
        let _ = rx.try_recv(); // consume work_item create event
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            31,
            "work_item.transition",
            json!({
                "id": wi_id,
                "target_status": "Ready",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Ready");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "work_item");
        assert_eq!(event.data["from"], "Draft");
        assert_eq!(event.data["to"], "Ready");
    }

    #[test]
    fn test_work_item_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "WI"})),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Draft → InProgress (invalid: must go through Ready)
        let req = DaemonRequest::new(
            31,
            "work_item.transition",
            json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_work_item_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "work_item.create", json!({"phase_id": phase_id, "title": "WI"})),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition Draft → Ready
        let req = DaemonRequest::new(
            31,
            "work_item.transition",
            json!({
                "id": wi_id,
                "target_status": "Ready",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_work_item_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "work_item.transition",
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
    fn test_work_item_transition_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
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
                "work_item.create",
                json!({"phase_id": phase_id, "title": "Transition WI", "description": "Test"}),
            ),
        );
        assert!(!create_resp.is_error());
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft → Ready
        let req = DaemonRequest::new(
            3,
            "work_item.transition",
            json!({
                "id": wi_id,
                "target_status": "Ready",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Ready");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<WorkItem> = store.get(&wi_id).unwrap();
        assert!(retrieved.is_some());
        let wi = retrieved.unwrap();
        assert_eq!(wi.status, WorkItemStatus::Ready);
    }

    // --- bundle handlers ---

    /// Helper: create plan + spec + phase + work_item and return (phase_id, work_item_id)
    fn create_test_work_item(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String) {
        let (_plan_id, _spec_id, phase_id) = create_test_phase(stores, tx, wm);
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work_item.create",
                json!({"phase_id": phase_id, "title": "Parent WI"}),
            ),
        );
        let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (phase_id, wi_id)
    }

    #[test]
    fn test_bundle_create_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/persist",
                "base_tick_id": "tick-001",
                "claims": "Persisted bundle"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let bundle_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Bundle> = store.get(&bundle_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().claims, "Persisted bundle");
    }

    #[test]
    fn test_bundle_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": "tick-001",
                "claims": "Add JWT signing"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["work_item_id"], wi_id);
        assert_eq!(result["branch_name"], "feature/auth");
        assert_eq!(result["base_tick_id"], "tick-001");
        assert_eq!(result["claims"], "Add JWT signing");
        assert_eq!(result["status"], "Proposed");
        assert_eq!(stores.bundles.read().unwrap().len(), 1);
    }

    #[test]
    fn test_bundle_create_no_base_tick() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/init"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["base_tick_id"].is_null());
    }

    #[test]
    fn test_bundle_create_missing_work_item_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "bundle.create", json!({"branch_name": "feature/x"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("work_item_id"));
    }

    #[test]
    fn test_bundle_create_work_item_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "bundle.create",
            json!({"work_item_id": "nonexistent", "branch_name": "feature/x"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_bundle_create_missing_branch_name() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);
        let req = DaemonRequest::new(40, "bundle.create", json!({"work_item_id": wi_id, "claims": "stuff"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("branch_name"));
    }

    #[test]
    fn test_bundle_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);
        // Drain plan+spec+phase+work_item create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_item_id": wi_id, "branch_name": "feature/x"}),
        );
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "bundle");
    }

    #[test]
    fn test_bundle_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id, "branch_name": "feature/auth"}),
            ),
        );
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(41, "bundle.get", json!({"id": bundle_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["branch_name"], "feature/auth");
    }

    #[test]
    fn test_bundle_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "bundle.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_bundle_get_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);

        // Create a bundle (writes to both TaskStore and HashMap)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id, "branch_name": "feature/ts-read"}),
            ),
        );
        assert!(!create_resp.is_error());
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.bundles.write().unwrap().remove(&bundle_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(41, "bundle.get", json!({"id": bundle_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["branch_name"], "feature/ts-read");
    }

    #[test]
    fn test_bundle_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "bundle.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_bundle_list_filtered_by_work_item_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id_1) = create_test_work_item(&stores, &tx, &wm);

        // Create a second work item under the same phase
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "work_item.create", json!({"phase_id": _phase_id, "title": "WI 2"})),
        );
        let wi_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create bundles under different work items
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id_1, "branch_name": "feature/a"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                41,
                "bundle.create",
                json!({"work_item_id": wi_id_2, "branch_name": "feature/b"}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "bundle.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by wi_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "bundle.list", json!({"work_item_id": wi_id_1})),
        );
        let bundles = filtered_resp.result.unwrap();
        let arr = bundles.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["branch_name"], "feature/a");
    }

    #[test]
    fn test_bundle_list_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id_1) = create_test_work_item(&stores, &tx, &wm);

        // Create a second work item under the same phase
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "work_item.create", json!({"phase_id": _phase_id, "title": "WI 2"})),
        );
        let wi_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create bundles under different work items (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id_1, "branch_name": "feature/a"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                41,
                "bundle.create",
                json!({"work_item_id": wi_id_2, "branch_name": "feature/b"}),
            ),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.bundles.write().unwrap().clear();

        // List all should still return both bundles via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "bundle.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(51, "bundle.list", json!({"work_item_id": wi_id_1}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        let arr = filtered_items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["branch_name"], "feature/a");
    }

    #[test]
    fn test_bundle_transition_proposed_to_triaged() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);
        // Drain plan+spec+phase+work_item create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id, "branch_name": "feature/x"}),
            ),
        );
        let _ = rx.try_recv(); // consume bundle create event
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            41,
            "bundle.transition",
            json!({
                "id": bundle_id,
                "target_status": "Triaged",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Triaged");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "bundle");
        assert_eq!(event.data["from"], "Proposed");
        assert_eq!(event.data["to"], "Triaged");
    }

    #[test]
    fn test_bundle_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id, "branch_name": "feature/x"}),
            ),
        );
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Proposed → Accepted (invalid: must go through Triaged → Reviewed)
        let req = DaemonRequest::new(
            41,
            "bundle.transition",
            json!({
                "id": bundle_id,
                "target_status": "Accepted",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_bundle_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_item_id": wi_id, "branch_name": "feature/x"}),
            ),
        );
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition Proposed → Triaged
        let req = DaemonRequest::new(
            41,
            "bundle.transition",
            json!({
                "id": bundle_id,
                "target_status": "Triaged",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_bundle_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "bundle.transition",
            json!({
                "id": "nonexistent",
                "target_status": "Triaged"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    // --- Staleness guard tests ---

    /// Helper: insert a Published Tick into the store and return its ID.
    fn insert_published_tick(stores: &Arc<Stores>, number: u32) -> String {
        use crate::domain::tick::Tick;
        let mut tick = Tick::new(number);
        tick.status = TickStatus::Published;
        tick.integration_sha = Some(format!("sha-{number}"));
        let id = tick.id.clone();
        stores.ticks.write().unwrap().insert(id.clone(), tick);
        id
    }

    #[test]
    fn test_bundle_create_staleness_guard_rejects_no_base_tick_when_published_exists() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);
        let _tick_id = insert_published_tick(&stores, 1);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/auth"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32002);
        assert!(err.message.contains("staleness guard"));
    }

    #[test]
    fn test_bundle_create_staleness_guard_rejects_stale_base_tick() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);
        let _old_tick_id = insert_published_tick(&stores, 1);
        let latest_tick_id = insert_published_tick(&stores, 2);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": "old-stale-tick-id"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32002);
        assert!(err.message.contains("staleness guard"));
        assert!(err.message.contains(&latest_tick_id));
    }

    #[test]
    fn test_bundle_create_staleness_guard_accepts_matching_base_tick() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);
        let tick_id = insert_published_tick(&stores, 1);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": tick_id,
                "claims": "Add auth"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "Expected success but got: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["base_tick_id"], tick_id);
        assert_eq!(result["status"], "Proposed");
    }

    #[test]
    fn test_bundle_create_staleness_guard_uses_highest_tick_number() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);
        let _tick1_id = insert_published_tick(&stores, 1);
        let tick2_id = insert_published_tick(&stores, 2);

        // Using tick1's ID should be rejected (tick2 is latest)
        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": _tick1_id,
                "claims": "Add auth"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains(&tick2_id));
    }

    #[test]
    fn test_bundle_create_staleness_guard_broadcasts_stale_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);
        // Drain create events
        while rx.try_recv().is_ok() {}

        let _tick_id = insert_published_tick(&stores, 1);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": "stale-id"
            }),
        );
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "bundle.rejected_stale");
        assert_eq!(event.data["bundle_work_item_id"], wi_id.as_str());
        assert_eq!(event.data["base_tick_id"], "stale-id");
    }

    #[test]
    fn test_bundle_create_bootstrap_no_published_tick_no_base() {
        // Bootstrap case: no published tick, no base_tick_id → OK
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_item_id": wi_id,
                "branch_name": "feature/init"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
    }

    // --- Tick handler tests ---

    #[test]
    fn test_tick_create_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(50, "tick.create", json!({"number": 7}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Tick> = store.get(&tick_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().number, 7);
    }

    #[test]
    fn test_tick_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(50, "tick.create", json!({"number": 1}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["number"], 1);
        assert_eq!(result["status"], "Open");
        assert!(result["integration_sha"].is_null());
        assert_eq!(result["bundle_ids"].as_array().unwrap().len(), 0);
        assert_eq!(stores.ticks.read().unwrap().len(), 1);
    }

    #[test]
    fn test_tick_create_missing_number() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tick.create", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("number"));
    }

    #[test]
    fn test_tick_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();

        let req = DaemonRequest::new(50, "tick.create", json!({"number": 1}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "tick");
    }

    #[test]
    fn test_tick_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 42})),
        );
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "tick.get", json!({"id": tick_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["number"], 42);
    }

    #[test]
    fn test_tick_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tick.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_tick_get_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a tick (writes to both TaskStore and HashMap)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 99})),
        );
        assert!(!create_resp.is_error());
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.ticks.write().unwrap().remove(&tick_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(51, "tick.get", json!({"id": tick_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["number"], 99);
    }

    #[test]
    fn test_tick_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tick.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_tick_list_filtered_by_status() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create two ticks
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let create2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "tick.create", json!({"number": 2})),
        );
        let tick2_id = create2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition tick 2 to Sealing
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": tick2_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(60, "tick.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by Open — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(61, "tick.list", json!({"status": "Open"})),
        );
        let ticks = filtered_resp.result.unwrap();
        let arr = ticks.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["number"], 1);
    }

    #[test]
    fn test_tick_list_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create two ticks (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "tick.create", json!({"number": 2})),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.ticks.write().unwrap().clear();

        // List all should still return both ticks via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(60, "tick.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list by status also works from TaskStore
        // Both ticks are Open (created in Open status)
        let filtered_req = DaemonRequest::new(61, "tick.list", json!({"status": "Open"}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        let arr = filtered_items.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn test_tick_transition_open_to_sealing() {
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
        );
        let _ = rx.try_recv(); // consume create event
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing", "role": "integrator"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Sealing");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "tick");
        assert_eq!(event.data["from"], "Open");
        assert_eq!(event.data["to"], "Sealing");
    }

    #[test]
    fn test_tick_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Open → Published (invalid: must go through Sealing → Validating)
        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Published", "role": "integrator"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_tick_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Coordinator cannot transition tick (Integrator-only)
        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing", "role": "coordinator"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_tick_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tick.transition",
            json!({"id": "nonexistent", "target_status": "Sealing"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_tick_transition_default_role_is_integrator() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Omit role — should default to Integrator and succeed
        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Sealing");
    }

    // --- learning.create tests ---

    fn create_learning(
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
                    "scope": "workitem",
                    "content": "Always run tests"
                }),
            ),
        );
        assert!(!resp.is_error());
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_learning_create_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "learning.create",
            json!({
                "source_id": "wi-123",
                "scope": "workitem",
                "content": "Always run tests"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
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

    #[test]
    fn test_learning_create_success() {
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
                    "scope": "workitem",
                    "content": "Always run tests before committing"
                }),
            ),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["source_id"], "wi-123");
        assert_eq!(result["scope"], "workitem");
        assert_eq!(result["content"], "Always run tests before committing");
        assert_eq!(result["reinforcements"], 0);
        assert!(!result["promoted"].as_bool().unwrap());
        assert_eq!(stores.learnings.read().unwrap().len(), 1);
    }

    #[test]
    fn test_learning_create_missing_source_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.create", json!({"scope": "global", "content": "insight"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("source_id"));
    }

    #[test]
    fn test_learning_create_missing_content() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.create", json!({"source_id": "wi-1", "scope": "global"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("content"));
    }

    #[test]
    fn test_learning_create_invalid_scope() {
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
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("scope"));
    }

    #[test]
    fn test_learning_create_broadcasts_event() {
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
        );
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "learning");
    }

    // --- learning.get tests ---

    #[test]
    fn test_learning_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.get", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["source_id"], "wi-123");
    }

    #[test]
    fn test_learning_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.get", json!({"id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_learning_get_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a learning (writes to both TaskStore and HashMap)
        let learning_id = create_learning(&stores, &tx, &wm, 50);

        // Remove from HashMap to prove get reads from TaskStore
        stores.learnings.write().unwrap().remove(&learning_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(51, "learning.get", json!({"id": learning_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["source_id"], "wi-123");
    }

    // --- learning.list tests ---

    #[test]
    fn test_learning_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.list", json!(null)),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_learning_list_with_scope_filter() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create a workitem-scoped learning
        create_learning(&stores, &tx, &wm, 1);
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
        );

        // Filter by global scope
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.list", json!({"scope": "global"})),
        );
        assert!(!resp.is_error());
        let list = resp.result.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["scope"], "global");
    }

    #[test]
    fn test_learning_list_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a workitem-scoped learning (writes to both TaskStore and HashMap)
        create_learning(&stores, &tx, &wm, 1);
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
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.learnings.write().unwrap().clear();

        // List all should still return both learnings via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "learning.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list by scope works from TaskStore
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "learning.list", json!({"scope": "global"})),
        );
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        assert_eq!(filtered_items.as_array().unwrap().len(), 1);
        assert_eq!(filtered_items[0]["scope"], "global");
    }

    // --- learning.reinforce tests ---

    #[test]
    fn test_learning_reinforce() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.reinforce", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["reinforcements"], 1);

        // Reinforce again
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.reinforce", json!({"id": learning_id})),
        );
        assert_eq!(resp2.result.unwrap()["reinforcements"], 2);
    }

    #[test]
    fn test_learning_reinforce_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.reinforce", json!({"id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    // --- learning.contradict tests ---

    #[test]
    fn test_learning_contradict() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.contradict", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["contradictions"], 1);
    }

    // --- learning.promote / demote tests ---

    #[test]
    fn test_learning_promote_and_demote() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        // Promote
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert!(resp.result.unwrap()["promoted"].as_bool().unwrap());

        // Demote
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.demote", json!({"id": learning_id})),
        );
        assert!(!resp2.is_error());
        assert!(!resp2.result.unwrap()["promoted"].as_bool().unwrap());
    }

    #[test]
    fn test_learning_promote_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let learning_id = create_learning(&stores, &tx, &wm, 1);
        let _ = rx.try_recv(); // consume create event

        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.updated");
        assert_eq!(event.data["collection"], "learning");
    }

    #[test]
    fn test_learning_reinforce_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.reinforce", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert_eq!(learning.unwrap().reinforcements, 1);
    }

    #[test]
    fn test_learning_contradict_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.contradict", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert_eq!(learning.unwrap().contradictions, 1);
    }

    #[test]
    fn test_learning_promote_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert!(learning.unwrap().promoted);
    }

    #[test]
    fn test_learning_demote_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        // Promote first so we can demote
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.demote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert!(!learning.unwrap().promoted);
    }

    // --- Lock handler tests ---

    fn create_lock(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>, wm: &WorktreeManager, id: u64) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                id,
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord-1"}),
            ),
        );
        assert!(!resp.is_error());
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_lock_create_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "lock.create",
            json!({
                "resource": "src/main.rs",
                "holder_id": "wi-1",
                "granted_by": "coord-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let lock_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Lock> = store.get(&lock_id).unwrap();
        assert!(retrieved.is_some());
        let lock = retrieved.unwrap();
        assert_eq!(lock.resource, "src/main.rs");
        assert_eq!(lock.holder_id, "wi-1");
        assert_eq!(lock.granted_by, "coord-1");
    }

    #[test]
    fn test_lock_create() {
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
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord-1"}),
            ),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["resource"], "src/main.rs");
        assert_eq!(result["holder_id"], "wi-1");
        assert_eq!(result["granted_by"], "coord-1");
        assert_eq!(result["status"], "active");
    }

    #[test]
    fn test_lock_create_missing_resource() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "lock.create", json!({"holder_id": "wi-1", "granted_by": "coord-1"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_create_missing_holder_id() {
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
                "lock.create",
                json!({"resource": "file.rs", "granted_by": "coord-1"}),
            ),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_get() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.get", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["resource"], "src/main.rs");
    }

    #[test]
    fn test_lock_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "lock.get", json!({"id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_get_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a lock (writes to both TaskStore and HashMap)
        let lock_id = create_lock(&stores, &tx, &wm, 50);

        // Remove from HashMap to prove get reads from TaskStore
        stores.locks.write().unwrap().remove(&lock_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(51, "lock.get", json!({"id": lock_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["resource"], "src/main.rs");
    }

    #[test]
    fn test_lock_list() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        create_lock(&stores, &tx, &wm, 1);
        create_lock(&stores, &tx, &wm, 2);
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.list", json!({})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_lock_list_filter_active_only() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        create_lock(&stores, &tx, &wm, 2);

        // Release the first lock
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.release", json!({"id": lock_id})),
        );

        // List active only
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "lock.list", json!({"active_only": true})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_lock_list_reads_from_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create two locks (writes to both TaskStore and HashMap)
        create_lock(&stores, &tx, &wm, 1);
        // Create a second lock with different resource
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "lock.create",
                json!({"resource": "src/lib.rs", "holder_id": "wi-2", "granted_by": "coord-1"}),
            ),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.locks.write().unwrap().clear();

        // List all should still return both locks via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "lock.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test active_only filter works from TaskStore (both are Active)
        let active_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "lock.list", json!({"active_only": true})),
        );
        assert!(!active_resp.is_error());
        assert_eq!(active_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test resource filter works from TaskStore
        let resource_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(12, "lock.list", json!({"resource": "src/lib.rs"})),
        );
        assert!(!resource_resp.is_error());
        let resource_items = resource_resp.result.unwrap();
        assert_eq!(resource_items.as_array().unwrap().len(), 1);
        assert_eq!(resource_items[0]["resource"], "src/lib.rs");
    }

    #[test]
    fn test_lock_release() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.release", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "released");
    }

    #[test]
    fn test_lock_release_already_released() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.release", json!({"id": lock_id})),
        );
        // Try releasing again
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.release", json!({"id": lock_id})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_expire() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.expire", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "expired");
    }

    #[test]
    fn test_lock_expire_already_expired() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.expire", json!({"id": lock_id})),
        );
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.expire", json!({"id": lock_id})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_release_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "lock.create",
            json!({
                "resource": "src/main.rs",
                "holder_id": "wi-1",
                "granted_by": "coord-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let lock_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "lock.release", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let lock: Option<Lock> = store.get(&lock_id).unwrap();
        assert!(lock.is_some());
        assert_eq!(lock.unwrap().status.to_string(), "Released");
    }

    #[test]
    fn test_lock_expire_persists_to_taskstore() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "lock.create",
            json!({
                "resource": "src/main.rs",
                "holder_id": "wi-1",
                "granted_by": "coord-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let lock_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "lock.expire", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let lock: Option<Lock> = store.get(&lock_id).unwrap();
        assert!(lock.is_some());
        assert_eq!(lock.unwrap().status.to_string(), "Expired");
    }

    #[test]
    fn test_lock_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        create_lock(&stores, &tx, &wm, 1);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "lock");
    }

    #[test]
    fn test_lock_release_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        let _ = rx.try_recv(); // consume create event

        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.release", json!({"id": lock_id})),
        );
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.updated");
        assert_eq!(event.data["collection"], "lock");
    }

    // --- Worktree handler tests ---

    #[test]
    fn test_worktree_create_missing_work_item_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.create", json!({})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("work_item_id"));
    }

    #[test]
    fn test_worktree_create_work_item_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.create", json!({"work_item_id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("not found"));
    }

    #[test]
    fn test_worktree_create_validates_work_item_exists() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create a full hierarchy so work_item exists
        let (_phase_id, wi_id) = create_test_work_item(&stores, &tx, &wm);
        // This will fail at the git level (nonexistent repo path) but should
        // pass the work_item validation
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "worktree.create", json!({"work_item_id": wi_id})),
        );
        // The error should be from git, not from "not found"
        assert!(resp.is_error());
        let msg = &resp.error.as_ref().unwrap().message;
        assert!(
            !msg.contains("not found"),
            "error should be from git, not from validation: {}",
            msg
        );
    }

    #[test]
    fn test_worktree_list_returns_response() {
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
        );
        // Will be an error since the repo doesn't exist, but the method routes
        assert!(resp.is_error());
    }

    #[test]
    fn test_worktree_cleanup_missing_work_item_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.cleanup", json!({})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("work_item_id"));
    }

    #[test]
    fn test_worktree_cleanup_work_item_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.cleanup", json!({"work_item_id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("not found"));
    }

    #[test]
    fn test_worktree_refresh_missing_work_item_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.refresh", json!({})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("work_item_id"));
    }

    #[test]
    fn test_worktree_refresh_nonexistent_worktree() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.refresh", json!({"work_item_id": "nonexistent"})),
        );
        // Will error since worktree path doesn't exist
        assert!(resp.is_error());
    }

    #[test]
    fn test_worktree_dispatch_routes_all_methods() {
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
            );
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

    // --- integrator.validate tests ---

    fn create_sealing_tick(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>, wm: &WorktreeManager) -> String {
        // Create a tick
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        );
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        // Transition Open → Sealing
        let tr = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "tick.transition",
                json!({"id": tick_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );
        assert!(!tr.is_error(), "transition failed: {:?}", tr.error);
        tick_id
    }

    #[test]
    fn test_integrator_validate_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        // Use "echo ok" as validation command (always succeeds)
        let ic = IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.validate", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error(), "unexpected error: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Published");
        assert!(!result["validation_log"].as_str().unwrap().is_empty());
        // integration_sha should be set (we're in a git repo)
        assert!(result["integration_sha"].is_string());
    }

    #[test]
    fn test_integrator_validate_failure() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        // Use a failing command
        let ic = IntegratorConfig {
            validation_commands: vec!["false".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.validate", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Failed");
        assert!(result["validation_log"].as_str().unwrap().contains("FAILED"));
        assert!(result["integration_sha"].is_null());
    }

    #[test]
    fn test_integrator_validate_wrong_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a tick in Open state (not Sealing)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        );
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "integrator.validate", json!({"tick_id": tick_id})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Sealing"));
    }

    #[test]
    fn test_integrator_validate_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "integrator.validate", json!({"tick_id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("not found"));
    }

    #[test]
    fn test_integrator_validate_missing_tick_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "integrator.validate", json!({})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("tick_id"));
    }

    #[test]
    fn test_integrator_validate_events() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);
        // Drain setup events
        while rx.try_recv().is_ok() {}

        let ic = IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        };
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.validate", json!({"tick_id": tick_id})),
        );
        // Should get transition.completed (Sealing→Validating) and tick.published events
        let event1 = rx.try_recv().unwrap();
        assert_eq!(event1.event, "transition.completed");
        let event2 = rx.try_recv().unwrap();
        assert_eq!(event2.event, "tick.published");
    }

    // --- integrator.publish tests ---

    #[test]
    fn test_integrator_publish_from_open() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a tick in Open state
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        );
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // publish chains Open → Sealing → Validating → Published
        let ic = IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(2, "integrator.publish", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Published");
    }

    #[test]
    fn test_integrator_publish_from_sealing() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        let ic = IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.publish", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Published");
    }

    #[test]
    fn test_integrator_publish_wrong_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        // Manually transition to Validating to create wrong state
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "tick.transition",
                json!({"id": tick_id, "target_status": "Validating", "role": "integrator"}),
            ),
        );

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "integrator.publish", json!({"tick_id": tick_id})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Open or Sealing"));
    }

    #[test]
    fn test_integrator_publish_validation_failure() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        );
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let ic = IntegratorConfig {
            validation_commands: vec!["false".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(2, "integrator.publish", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Failed");
    }

    #[test]
    fn test_integrator_dispatch_routes() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        for method in &["integrator.validate", "integrator.publish"] {
            let resp = dispatch(
                &stores,
                &tx,
                &wm,
                &test_integrator_config(),
                DaemonRequest::new(1, *method, json!({})),
            );
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

    #[test]
    fn test_integrator_validate_multi_command_stops_on_first_failure() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        let ic = IntegratorConfig {
            validation_commands: vec![
                "echo first".to_string(),
                "false".to_string(),
                "echo should-not-run".to_string(),
            ],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.validate", json!({"tick_id": tick_id})),
        );
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Failed");
        let log = result["validation_log"].as_str().unwrap();
        assert!(log.contains("first"));
        assert!(log.contains("FAILED"));
        assert!(!log.contains("should-not-run"));
    }

    // --- system.init tests ---

    #[test]
    fn test_dispatch_system_init() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.init", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "system.init failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        let collections = result["collections"].as_array().unwrap();
        assert_eq!(collections.len(), 9);
        assert!(collections.contains(&json!("plans")));
        assert!(collections.contains(&json!("specs")));
        assert!(collections.contains(&json!("phases")));
        assert!(collections.contains(&json!("work_items")));
        assert!(collections.contains(&json!("bundles")));
        assert!(collections.contains(&json!("ticks")));
        assert!(collections.contains(&json!("learnings")));
        assert!(collections.contains(&json!("locks")));
        assert!(collections.contains(&json!("agent_sessions")));
        // git_hooks_installed is best-effort — may be false in test environments
        // due to taskstore's configure_merge_driver not using current_dir
        assert!(result.get("git_hooks_installed").is_some());
    }

    #[test]
    fn test_dispatch_system_init_without_store() {
        let stores = test_stores(); // No TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.init", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert!(err.message.contains("TaskStore not initialized"));
    }

    #[test]
    fn test_dispatch_system_init_idempotent() {
        let stores = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Call init twice — should succeed both times
        let req1 = DaemonRequest::new(1, "system.init", json!({}));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());
        let req2 = DaemonRequest::new(2, "system.init", json!({}));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp2.is_error());
    }

    // --- Validation gate tests (Phase 4) ---

    #[test]
    fn test_plan_transition_blocked_no_report_when_validator_enabled() {
        let stores = test_stores_with_validator();
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

        // Try Draft → Active without any validation report — should be blocked
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
        let stores = test_stores_with_validator();
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

        // Draft → Active should succeed
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
        let stores = test_stores_with_validator();
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

        // Draft → Active should succeed (Warn allows transition)
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
        let stores = test_stores_with_validator();
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

        // Draft → Active should be blocked
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
        let stores = test_stores_with_validator();
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

        // Draft → Active with skip_validation=true — should succeed even without report
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
        // test_stores_with_taskstore has no validator → gate should not apply
        let stores = test_stores_with_taskstore();
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

        // Draft → Active should succeed without any validation report
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
    fn test_spec_transition_blocked_no_report_when_validator_enabled() {
        let stores = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create parent plan
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create spec
        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Gate Test Spec"})),
        );
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft → Active without report — blocked
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
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32003);
    }

    #[test]
    fn test_spec_transition_allowed_with_pass_report() {
        let stores = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Gate Test Spec"})),
        );
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

        // Draft → Active should succeed
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
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_phase_transition_blocked_no_report_when_validator_enabled() {
        let stores = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create parent plan → spec → phase
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Parent Spec"})),
        );
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
                    "spec_id": spec_id, "title": "Gate Test Phase", "order": 1
                }),
            ),
        );
        let phase_id = phase_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft → Active without report — blocked
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
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32003);
    }

    #[test]
    fn test_phase_transition_skip_validation_override() {
        let stores = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Parent Spec"})),
        );
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
                    "spec_id": spec_id, "title": "Gate Test Phase", "order": 1
                }),
            ),
        );
        let phase_id = phase_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft → Active with skip_validation — should succeed
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
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_non_draft_to_active_transition_no_gate() {
        // Active → Complete should NOT trigger the validation gate
        let stores = test_stores_with_validator();
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

        // First, skip validation to get to Active
        dispatch(
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

        // Active → Complete should work without any validation report
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "complete",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "complete");
    }

    #[test]
    fn test_latest_report_wins_for_validation_gate() {
        let stores = test_stores_with_validator();
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

        // Insert a Fail report first (older)
        let fail_report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Fail,
            vec![],
            "Failed".to_string(),
            "test-model".to_string(),
        );
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create(fail_report)
            .unwrap();

        // Sleep briefly to ensure different timestamps
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Insert a Pass report (newer — should win)
        let pass_report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Pass,
            vec![],
            "Passed".to_string(),
            "test-model".to_string(),
        );
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create(pass_report)
            .unwrap();

        // Draft → Active should succeed (latest report is Pass)
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
}
