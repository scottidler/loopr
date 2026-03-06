use std::process::Command;
use std::sync::Arc;

use eyre::eyre;
use log::{debug, info};
use serde_json::json;
use tokio::sync::broadcast;

use crate::agents::{AgentSession, AgentStatus, AgentType};
use crate::config::IntegratorConfig;
use crate::domain::bundle::{Bundle, BundleStatus, bundle_transitions};
use crate::domain::coordinator_goal::CoordinatorGoal;
use crate::domain::learning::{Learning, LearningScope};
use crate::domain::lock::Lock;
use crate::domain::phase::{Phase, PhaseStatus};
use crate::domain::plan::{HierarchyStatus, Plan, PlanStatus, hierarchy_transitions};
use crate::domain::role::Role;
use crate::domain::spec::{Spec, SpecStatus};
use crate::domain::tick::{Tick, TickStatus, tick_transitions};
use crate::domain::transition::validate_transition;
use crate::domain::validation::{ValidationReport, ValidationVerdict};
use crate::domain::work::{Work, WorkStatus, override_transitions, work_transitions};
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::worktree::manager::WorktreeManager;

use taskstore::{Filter, FilterOp, IndexValue, Record};

use super::context::Stores;

/// Convert a handler body that returns Result<DaemonResponse> into a DaemonResponse,
/// mapping any Err into an RPC internal error response.
macro_rules! try_handler {
    ($req_id:expr, $body:expr) => {{
        #[allow(clippy::redundant_closure_call)]
        let __result = (|| -> eyre::Result<DaemonResponse> { $body })();
        match __result {
            Ok(resp) => resp,
            Err(e) => DaemonResponse::err($req_id, RpcError::internal(&e.to_string())),
        }
    }};
}

/// Returns the configured max_pool for a given agent type.
fn max_pool_for(agent_type: AgentType, config: &crate::config::Config) -> u32 {
    match agent_type {
        AgentType::Implementer => config.agents.implementer.max_pool,
        AgentType::Reviewer => config.agents.reviewer.max_pool,
        AgentType::Coordinator => config.agents.coordinator.role.max_pool,
        AgentType::Researcher => config.agents.researcher.max_pool,
        AgentType::Integrator => 1,
        AgentType::Chat => 1, // Single chat session for now
    }
}

/// Check the validation gate for Draft → Active transitions.
/// Returns `Some(RpcError)` if the gate blocks the transition, `None` if allowed.
/// Gate only applies when:
/// 1. Validator is enabled (stores.validator is Some)
/// 2. Transition is Draft → Active
/// 3. skip_validation param is not true
#[allow(clippy::too_many_arguments)]
fn check_validation_gate(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    from: HierarchyStatus,
    target: HierarchyStatus,
    collection: &str,
    id: &str,
    skip_validation: bool,
    skip_reason: Option<&str>,
) -> Option<RpcError> {
    // Gate only applies to Draft → Active
    if from != HierarchyStatus::Draft || target != HierarchyStatus::Active {
        return None;
    }

    // Gate only applies when validator is enabled
    stores.validator.as_ref()?;

    // Coordinator can skip validation with explicit flag
    if skip_validation {
        // Gap #8: Audit trail for skip-validation
        let reason = skip_reason.unwrap_or("no reason given");
        let _ = event_tx.send(DaemonEvent::new(
            "validation.skipped",
            json!({"collection": collection, "id": id, "reason": reason}),
        ));
        return None;
    }

    // Check for a passing ValidationReport in TaskStore
    if let Some(store) = &stores.store {
        let Ok(store) = store.lock() else {
            return Some(RpcError::internal("taskstore lock poisoned"));
        };
        let reports: Vec<ValidationReport> = store
            .list(&[Filter {
                field: "target_id".into(),
                op: FilterOp::Eq,
                value: IndexValue::String(id.to_string()),
            }])
            .unwrap_or_default();

        // Find the latest report (highest updated_at)
        let latest = reports.iter().max_by_key(|r| r.created_at);

        // Gap #23: Apply ValidatorStrictness
        let strictness = stores.config.strategy.validator_strictness;
        match latest {
            Some(report) => match report.verdict {
                ValidationVerdict::Fail => match strictness {
                    crate::config::ValidatorStrictness::SuggestOnly => None,
                    _ => Some(RpcError::validation_required(collection, id)),
                },
                ValidationVerdict::Warn => match strictness {
                    crate::config::ValidatorStrictness::HardFailOnAnyAmbiguity => {
                        Some(RpcError::validation_required(collection, id))
                    }
                    _ => None,
                },
                ValidationVerdict::Pass => None,
            },
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
    debug!("dispatch(method={})", req.method);
    let method = req.method.clone();
    let params = req.params.clone();
    let resp = match req.method.as_str() {
        "system.handshake" => handle_handshake(stores, req),
        "system.init" => handle_system_init(stores, req),
        "system.status" => handle_status(stores, req),
        "system.shutdown" => handle_shutdown(event_tx, req),
        "plan.create" => handle_plan_create(stores, event_tx, req),
        "plan.get" => handle_plan_get(stores, req),
        "plan.list" => handle_plan_list(stores, req),
        "plan.transition" => handle_plan_transition(stores, event_tx, req),
        "plan.update" => handle_plan_update(stores, event_tx, req),
        "spec.create" => handle_spec_create(stores, event_tx, req),
        "spec.get" => handle_spec_get(stores, req),
        "spec.list" => handle_spec_list(stores, req),
        "spec.transition" => handle_spec_transition(stores, event_tx, req),
        "spec.update" => handle_spec_update(stores, event_tx, req),
        "phase.create" => handle_phase_create(stores, event_tx, req),
        "phase.get" => handle_phase_get(stores, req),
        "phase.list" => handle_phase_list(stores, req),
        "phase.transition" => handle_phase_transition(stores, event_tx, req),
        "phase.update" => handle_phase_update(stores, event_tx, req),
        "work.create" => handle_work_create(stores, event_tx, req),
        "work.get" => handle_work_get(stores, req),
        "work.list" => handle_work_list(stores, req),
        "work.transition" => handle_work_transition(stores, event_tx, req),
        "work.update" => handle_work_update(stores, event_tx, req),
        "bundle.create" => handle_bundle_create(stores, event_tx, req),
        "bundle.get" => handle_bundle_get(stores, req),
        "bundle.list" => handle_bundle_list(stores, req),
        "bundle.transition" => handle_bundle_transition(stores, event_tx, req),
        "bundle.update" => handle_bundle_update(stores, event_tx, req),
        "tick.create" => handle_tick_create(stores, event_tx, req),
        "tick.get" => handle_tick_get(stores, req),
        "tick.list" => handle_tick_list(stores, req),
        "tick.transition" => handle_tick_transition(stores, event_tx, req),
        "tick.update" => handle_tick_update(stores, event_tx, req),
        "learning.create" => handle_learning_create(stores, event_tx, req),
        "learning.get" => handle_learning_get(stores, req),
        "learning.list" => handle_learning_list(stores, req),
        "learning.update" => handle_learning_update(stores, event_tx, req),
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
        "coverage.evaluate" => handle_coverage_evaluate(stores, req),
        "tool.list" => handle_tool_list(stores, req),
        "coordinator.set_goal" => handle_coordinator_set_goal(stores, event_tx, req),
        "coordinator.clear_goal" => handle_coordinator_clear_goal(stores, event_tx, req),
        "coordinator.get_goal" => handle_coordinator_get_goal(stores, req),
        "coordinator.get_state" => handle_coordinator_get_state(stores, req),
        "coordinator.reset_state" => handle_coordinator_reset_state(stores, event_tx, req),
        "coordinator.interview_respond" => handle_coordinator_interview_respond(stores, event_tx, req),
        "coordinator.accept_plan" => handle_coordinator_accept_plan(stores, event_tx, req),
        "coordinator.interview_question" => handle_coordinator_interview_question(stores, event_tx, req),
        "chat.submit" => handle_chat_submit(stores, event_tx, req),
        "chat.attach" => handle_chat_attach(stores, req),
        "chat.history" => handle_chat_history(stores, req),
        "agent.start" => handle_agent_start(stores, event_tx, worktree_mgr, req),
        "agent.stop" => handle_agent_stop(stores, event_tx, req),
        "agent.pause" => handle_agent_pause(stores, event_tx, req),
        "agent.resume" => handle_agent_resume(stores, event_tx, req),
        "agent.status" => handle_agent_status(stores, req),
        "agent.list" => handle_agent_list(stores, req),
        "agent.output" => handle_agent_output(stores, req),
        _ => DaemonResponse::err(req.id, RpcError::method_not_found(&req.method)),
    };

    // Gap #29: Post-dispatch auto-start hook (only on successful transitions)
    if !resp.is_error() {
        auto_start_agents(stores, event_tx, worktree_mgr, integrator_config, &method, &params);
    }

    resp
}

/// Auto-start agents based on transition outcomes (Gap #29).
fn auto_start_agents(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    method: &str,
    params: &serde_json::Value,
) {
    if method == "work.transition"
        && let Some(target) = params.get("target_status").and_then(|v| v.as_str())
        && target == "InProgress"
        && stores.config.agents.auto_start_implementer
        && !stores.config.agents.pull_based_workers  // Workers handle their own spawning
        && let Some(wi_id) = params.get("id").and_then(|v| v.as_str())
    {
        let start_req = DaemonRequest::new(
            0,
            "agent.start",
            json!({
                "agent_type": "implementer", "work_id": wi_id,
            }),
        );
        let _ = dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
    }
    if method == "bundle.transition"
        && let Some(target) = params.get("target_status").and_then(|v| v.as_str())
        && target == "Triaged"
        && stores.config.agents.auto_start_reviewer
        && let Some(bid) = params.get("id").and_then(|v| v.as_str())
    {
        let start_req = DaemonRequest::new(
            0,
            "agent.start",
            json!({
                "agent_type": "reviewer", "bundle_id": bid,
            }),
        );
        let _ = dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
    }
}

fn handle_handshake(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_handshake()");
        let server_version = crate::version();
        let client_version = req
            .params
            .get("client_version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let version_match = client_version == server_version;
        if !version_match {
            log::warn!(
                "Client version mismatch: client={}, server={}",
                client_version,
                server_version
            );
        }

        Ok(DaemonResponse::ok(
            req.id,
            json!({
                "server_version": server_version,
                "client_version": client_version,
                "version_match": version_match,
                "protocol": "ndjson/1",
                "session_id": stores.session_id,
            }),
        ))
    })
}

fn handle_system_init(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_system_init()");
        let store_arc = match &stores.store {
            Some(s) => s,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::internal("TaskStore not initialized"),
                ));
            }
        };

        // Install git merge driver and .gitattributes (best-effort)
        let git_hooks_ok = {
            let store = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))?;
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
            Work::collection_name(),
            Bundle::collection_name(),
            Tick::collection_name(),
            Learning::collection_name(),
            Lock::collection_name(),
            CoordinatorGoal::collection_name(),
            AgentSession::collection_name(),
        ];

        Ok(DaemonResponse::ok(
            req.id,
            json!({ "collections": collections, "git_hooks_installed": git_hooks_ok }),
        ))
    })
}

fn handle_status(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_status()");
        let plans = stores.read_plans()?.len();
        let specs = stores.read_specs()?.len();
        let phases = stores.read_phases()?.len();
        let works = stores.read_works()?.len();
        let bundles = stores.read_bundles()?.len();
        let ticks = stores.read_ticks()?.len();
        let learnings = stores.read_learnings()?.len();
        let locks = stores.read_locks()?.len();
        let agent_sessions = stores.read_agent_sessions()?.len();

        // TaskStore stats (when available)
        let taskstore_stats = if let Some(store) = &stores.store {
            let s = store.lock().map_err(|_| eyre!("taskstore lock poisoned"))?;
            let ts_plans = s.list::<Plan>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_specs = s.list::<Spec>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_phases = s.list::<Phase>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_works = s.list::<Work>(&[]).map(|v| v.len()).unwrap_or(0);
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
                    "works": ts_works,
                    "bundles": ts_bundles,
                    "ticks": ts_ticks,
                    "learnings": ts_learnings,
                    "locks": ts_locks,
                }
            })
        } else {
            json!({ "enabled": false })
        };

        // Gap #33: Current Tick SHA — find the latest Published tick
        let current_tick_sha: Option<String> = {
            let ticks_map = stores.read_ticks()?;
            ticks_map
                .values()
                .filter(|t| t.status == TickStatus::Published)
                .max_by_key(|t| t.number)
                .and_then(|t| t.integration_sha.clone())
        };

        // Gap #33: Latest published tick ID for staleness check
        let latest_tick_id: Option<String> = {
            let ticks_map = stores.read_ticks()?;
            ticks_map
                .values()
                .filter(|t| t.status == TickStatus::Published)
                .max_by_key(|t| t.number)
                .map(|t| t.id.clone())
        };

        // Gap #33: Stale works count
        let stale_works: usize = {
            let wis = stores.read_works()?;
            let bundles_map = stores.read_bundles()?;
            if let Some(ref latest_tid) = latest_tick_id {
                wis.values()
                    .filter(|wi| wi.status == WorkStatus::InProgress)
                    .filter(|wi| {
                        bundles_map.values().any(|b| {
                            b.work_id == wi.id
                                && !matches!(
                                    b.status,
                                    BundleStatus::Merged | BundleStatus::Rejected | BundleStatus::Superseded
                                )
                                && b.base_tick_id.as_ref().is_some_and(|btid| btid != latest_tid)
                        })
                    })
                    .count()
            } else {
                0
            }
        };

        Ok(DaemonResponse::ok(
            req.id,
            json!({
                "version": crate::version(),
                "pid": std::process::id(),
                "counts": {
                    "plans": plans,
                    "specs": specs,
                    "phases": phases,
                    "works": works,
                    "bundles": bundles,
                    "ticks": ticks,
                    "learnings": learnings,
                    "locks": locks,
                    "agent_sessions": agent_sessions,
                },
                "taskstore": taskstore_stats,
                "current_tick_sha": current_tick_sha,
                "stale_works": stale_works,
                "session_id": stores.session_dir.as_ref().and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string())),
            }),
        ))
    })
}

fn handle_shutdown(event_tx: &broadcast::Sender<DaemonEvent>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_shutdown()");
        // Broadcast a shutdown event so the accept loop can pick it up
        let _ = event_tx.send(DaemonEvent::new("system.shutdown", json!({})));
        Ok(DaemonResponse::ok(req.id, json!({ "status": "shutting_down" })))
    })
}

fn handle_plan_create(
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
            if plans.values().any(|p| p.status == HierarchyStatus::Draft) {
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

fn handle_plan_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

fn handle_plan_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

fn handle_plan_transition(
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

        let target_status: PlanStatus = match req.params.get("target_status") {
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

        let from = plan.status;
        let rules = hierarchy_transitions();
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
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

        plan.status = target_status;
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

// --- Spec handlers ---

fn handle_spec_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_create()");
        let plan_id = match req.params.get("plan_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("plan_id is required"),
                ));
            }
        };

        // Verify parent plan exists and is not in a terminal state
        {
            let plans = stores.read_plans()?;
            match plans.get(&plan_id) {
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &plan_id))),
                Some(plan) if matches!(plan.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Cannot create spec under {} plan '{}'",
                            plan.status, plan_id
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

        // Reject if a Draft Spec already exists under this Plan
        {
            let specs = stores.read_specs()?;
            if specs
                .values()
                .any(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Draft)
            {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(
                        "A Draft Spec already exists under this Plan; abandon it before creating a new one",
                    ),
                ));
            }
        }

        let spec = Spec::new(plan_id, title, description);
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

        stores.write_specs()?.insert(id.clone(), spec);
        let _ = event_tx.send(DaemonEvent::record_created("spec", &id));

        Ok(DaemonResponse::ok(req.id, spec_json))
    })
}

fn handle_spec_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_get()");
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

fn handle_spec_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_list()");
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
            .filter(|s| plan_id_filter.is_none() || Some(s.plan_id.as_str()) == plan_id_filter)
            .collect();

        match serde_json::to_value(&spec_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

fn handle_spec_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: SpecStatus = match req.params.get("target_status") {
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

        let from = spec.status;
        let rules = hierarchy_transitions();
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
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

        spec.status = target_status;
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

// --- Phase handlers ---

fn handle_phase_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_create()");
        let spec_id = match req.params.get("spec_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("spec_id is required"),
                ));
            }
        };

        // Verify parent spec exists and is not in a terminal state
        {
            let specs = stores.read_specs()?;
            match specs.get(&spec_id) {
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &spec_id))),
                Some(spec) if matches!(spec.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Cannot create phase under {} spec '{}'",
                            spec.status, spec_id
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
                .any(|p| p.spec_id == spec_id && p.status == HierarchyStatus::Draft)
            {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(
                        "A Draft Phase already exists under this Spec; abandon it before creating a new one",
                    ),
                ));
            }
        }

        let phase = Phase::new(spec_id, title, description, order);
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

fn handle_phase_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_get()");
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

fn handle_phase_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_list()");
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
            .filter(|p| spec_id_filter.is_none() || Some(p.spec_id.as_str()) == spec_id_filter)
            .collect();

        match serde_json::to_value(&phase_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

fn handle_phase_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: PhaseStatus = match req.params.get("target_status") {
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

        let from = phase.status;
        let rules = hierarchy_transitions();
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
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

        phase.status = target_status;
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

// --- Work handlers ---

fn handle_work_create(
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

fn handle_work_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

fn handle_work_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

fn handle_work_transition(
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

// --- Bundle handlers ---

fn handle_bundle_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_bundle_create()");
        let work_id = match req.params.get("work_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("work_id is required"),
                ));
            }
        };

        // Verify parent work exists and is not in a terminal state
        {
            let works = stores.read_works()?;
            match works.get(&work_id) {
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("work", &work_id))),
                Some(work) if matches!(work.status, WorkStatus::Done | WorkStatus::Abandoned) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Cannot create bundle under {} work '{}'",
                            work.status, work_id
                        )),
                    ));
                }
                _ => {}
            }
        }

        let branch_name = req
            .params
            .get("branch_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if branch_name.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("branch_name is required"),
            ));
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
                let _ = event_tx.send(DaemonEvent::bundle_rejected_stale(&work_id, "(none)", &latest.id));
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::stale_bundle("(none)", &latest.id),
                ));
            }
            // Published tick exists and bundle's base_tick_id doesn't match it
            (Some(base_id), Some(latest)) if base_id != &latest.id => {
                let _ = event_tx.send(DaemonEvent::bundle_rejected_stale(&work_id, base_id, &latest.id));
                return Ok(DaemonResponse::err(req.id, RpcError::stale_bundle(base_id, &latest.id)));
            }
            // No published tick and no base_tick_id: bootstrap case, OK
            // base_tick_id matches latest published: OK
            _ => {}
        }

        // M1: Parse claims as array (backward-compat: also accepts string)
        let claims: Vec<String> = match req.params.get("claims") {
            Some(serde_json::Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            Some(serde_json::Value::String(s)) => {
                if s.is_empty() {
                    Vec::new()
                } else {
                    vec![s.clone()]
                }
            }
            _ => Vec::new(),
        };

        let description = req
            .params
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut bundle = Bundle::new(work_id, base_tick_id, branch_name, claims);
        bundle.description = description;

        // M8: Accept both "touched_paths" and "files_changed" (normalize param name)
        let touched_paths_val = req
            .params
            .get("touched_paths")
            .or_else(|| req.params.get("files_changed"));
        if let Some(files) = touched_paths_val.and_then(|v| v.as_array()) {
            bundle.touched_paths = files.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }

        // Gap #22: BundleSizePolicy enforcement on create
        if !bundle.touched_paths.is_empty() {
            let policy = &stores.config.strategy.bundle_size;
            if bundle.touched_paths.len() as u32 > policy.max_files_touched {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(&format!(
                        "Bundle touches {} files, exceeds max_files_touched={}",
                        bundle.touched_paths.len(),
                        policy.max_files_touched
                    )),
                ));
            }
        }

        let bundle_json = match serde_json::to_value(&bundle) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = bundle.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(bundle.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_bundles()?.insert(id.clone(), bundle);
        let _ = event_tx.send(DaemonEvent::record_created("bundle", &id));

        Ok(DaemonResponse::ok(req.id, bundle_json))
    })
}

fn handle_bundle_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_bundle_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Bundle>(id)
            {
                Ok(Some(bundle)) => {
                    return match serde_json::to_value(&bundle) {
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

        let bundles = stores.read_bundles()?;
        match bundles.get(id) {
            Some(bundle) => match serde_json::to_value(bundle) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("bundle", id))),
        }
    })
}

fn handle_bundle_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_bundle_list()");
        let wi_filter = req.params.get("work_id").and_then(|v| v.as_str());

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let filters: Vec<Filter> = if let Some(wid) = wi_filter {
                vec![Filter {
                    field: "work_id".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(wid.to_string()),
                }]
            } else {
                vec![]
            };
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Bundle>(&filters)
            {
                Ok(bundles) => {
                    return match serde_json::to_value(&bundles) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let bundles = stores.read_bundles()?;
        let bundle_list: Vec<&Bundle> = bundles
            .values()
            .filter(|b| wi_filter.is_none() || Some(b.work_id.as_str()) == wi_filter)
            .collect();

        match serde_json::to_value(&bundle_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

fn handle_bundle_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_bundle_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: BundleStatus = match req.params.get("target_status") {
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

        let mut bundles = stores.write_bundles()?;

        // Read bundle info first for validation
        let (from, bundle_wi_id, touched_paths, mut verification) = match bundles.get(&id) {
            Some(b) => (
                b.status,
                b.work_id.clone(),
                b.touched_paths.clone(),
                b.verification.clone(),
            ),
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("bundle", &id))),
        };

        // Allow setting verification during transition (e.g., Reviewer sets it when transitioning to Reviewed)
        if let Some(v) = req.params.get("verification").and_then(|v| v.as_str()) {
            verification = v.to_string();
        }

        let rules = bundle_transitions();
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
            let _ = event_tx.send(DaemonEvent::transition_rejected(
                "bundles",
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

        // #18: At most one Accepted Bundle per Work
        if target_status == BundleStatus::Accepted {
            let has_accepted = bundles
                .values()
                .any(|b| b.work_id == bundle_wi_id && b.id != id && b.status == BundleStatus::Accepted);
            if has_accepted {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("Work already has an Accepted Bundle"),
                ));
            }
        }

        // Gap #17: Bundle cannot touch locked resources it doesn't own
        if target_status == BundleStatus::Integrating {
            let locks = stores.read_locks()?;
            for path in &touched_paths {
                if let Some(lock) = locks.values().find(|l| l.resource == *path && l.is_active())
                    && lock.holder_id != bundle_wi_id
                {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Bundle touches locked resource '{}' owned by '{}'",
                            path, lock.holder_id
                        )),
                    ));
                }
            }
        }

        // Gap #18: Verification metadata required for Reviewed+
        if matches!(
            target_status,
            BundleStatus::Reviewed | BundleStatus::Accepted | BundleStatus::Integrating | BundleStatus::Merged
        ) && !matches!(
            from,
            BundleStatus::Reviewed | BundleStatus::Accepted | BundleStatus::Integrating
        ) && verification.is_empty()
        {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::precondition_failed("Bundle must have verification metadata before Reviewed+"),
            ));
        }

        // Now get mutable reference and apply the transition
        let bundle = bundles.get_mut(&id).ok_or_else(|| eyre!("record not found: {id}"))?;
        bundle.status = target_status;
        bundle.updated_at = crate::id::now_millis();
        // Apply verification from transition params if provided
        if !verification.is_empty() && bundle.verification.is_empty() {
            bundle.verification = verification;
        }
        let bundle_clone = bundle.clone();
        drop(bundles);

        // Persist transition to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(bundle_clone.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let bundle_json = match serde_json::to_value(&bundle_clone) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        debug!(
            "[transition] bundle.{}: {:?} -> {:?} by {}",
            id, from, target_status, role
        );
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "bundle",
            &id,
            &from.to_string(),
            &target_status.to_string(),
            &role.to_string(),
        ));

        Ok(DaemonResponse::ok(req.id, bundle_json))
    })
}

// --- Tick handlers ---

fn handle_tick_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_create()");
        // Singleton guard: at most one non-terminal Tick at a time
        {
            let ticks = stores.read_ticks()?;
            let active = ticks.values().any(|t| !t.status.is_terminal());
            if active {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("A non-terminal Tick already exists"),
                ));
            }
        }

        let number = match req.params.get("number").and_then(|v| v.as_u64()) {
            Some(n) => n as u32,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("number is required (positive integer)"),
                ));
            }
        };

        let tick = Tick::new(number);
        let tick_json = match serde_json::to_value(&tick) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = tick.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(tick.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_ticks()?.insert(id.clone(), tick);
        let _ = event_tx.send(DaemonEvent::record_created("tick", &id));

        Ok(DaemonResponse::ok(req.id, tick_json))
    })
}

fn handle_tick_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Tick>(id)
            {
                Ok(Some(tick)) => {
                    return match serde_json::to_value(&tick) {
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

        let ticks = stores.read_ticks()?;
        match ticks.get(id) {
            Some(tick) => match serde_json::to_value(tick) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", id))),
        }
    })
}

fn handle_tick_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_list()");
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
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Tick>(&filters)
            {
                Ok(ticks) => {
                    return match serde_json::to_value(&ticks) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let ticks = stores.read_ticks()?;
        let tick_list: Vec<&Tick> = ticks
            .values()
            .filter(|t| status_filter.is_none() || Some(t.status) == status_filter)
            .collect();

        match serde_json::to_value(&tick_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

fn handle_tick_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_transition()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: TickStatus = match req.params.get("target_status") {
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
            None => Role::Integrator,
        };

        let mut ticks = stores.write_ticks()?;

        // Read current status immutably first for validation
        let from = match ticks.get(&id) {
            Some(t) => t.status,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", &id))),
        };

        let rules = tick_transitions();
        if let Err(e) = validate_transition(from, target_status, role, &rules) {
            let _ = event_tx.send(DaemonEvent::transition_rejected(
                "ticks",
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

        // Gap #16: Only one Tick in Sealing/Validating at a time
        if matches!(target_status, TickStatus::Sealing | TickStatus::Validating) {
            let has_active = ticks
                .values()
                .any(|t| t.id != id && matches!(t.status, TickStatus::Sealing | TickStatus::Validating));
            if has_active {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("Another Tick is already in Sealing/Validating"),
                ));
            }
        }

        // Now get mutable reference and apply the transition
        let tick = ticks.get_mut(&id).ok_or_else(|| eyre!("record not found: {id}"))?;
        tick.status = target_status;
        tick.updated_at = crate::id::now_millis();
        let tick_clone = tick.clone();
        drop(ticks);

        // Persist transition to TaskStore if available (matches work_transition pattern)
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(tick_clone.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let tick_json = match serde_json::to_value(&tick_clone) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        debug!(
            "[transition] tick.{}: {:?} -> {:?} by {}",
            id, from, target_status, role
        );
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "tick",
            &id,
            &from.to_string(),
            &target_status.to_string(),
            &role.to_string(),
        ));

        Ok(DaemonResponse::ok(req.id, tick_json))
    })
}

// --- Learning handlers ---

fn handle_learning_create(
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

fn handle_learning_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

fn handle_learning_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

fn handle_learning_reinforce(
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

fn handle_learning_contradict(
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

fn handle_learning_promote(
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

fn handle_learning_demote(
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

// --- Lock handlers ---

fn handle_lock_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_create()");
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
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("resource is required"),
            ));
        }
        if holder_id.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("holder_id is required"),
            ));
        }
        if granted_by.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("granted_by is required"),
            ));
        }

        let mut lock = Lock::new(resource, holder_id, granted_by);

        // #11: Accept optional ttl_secs param; compute expires_at
        if let Some(ttl_secs) = req.params.get("ttl_secs").and_then(|v| v.as_u64()) {
            lock.expires_at = Some(crate::id::now_millis() + (ttl_secs as i64 * 1000));
        }

        // Gap #25: If no explicit TTL, apply max_lock_ttl_minutes from config
        if lock.expires_at.is_none() {
            let ttl_minutes = stores.config.strategy.max_lock_ttl_minutes;
            if ttl_minutes > 0 {
                lock.expires_at = Some(crate::id::now_millis() + (ttl_minutes as i64 * 60 * 1000));
            }
        }
        if let Some(renewable) = req.params.get("renewable").and_then(|v| v.as_bool()) {
            lock.renewable = renewable;
        }

        // Auto-expire any locks that have passed their TTL
        {
            let mut locks = stores.write_locks()?;
            for existing_lock in locks.values_mut() {
                if existing_lock.is_active() && existing_lock.is_expired() {
                    existing_lock.expire();
                }
            }
        }

        let lock_json = match serde_json::to_value(&lock) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = lock.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(lock.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_locks()?.insert(id.clone(), lock);
        let _ = event_tx.send(DaemonEvent::record_created("lock", &id));

        Ok(DaemonResponse::ok(req.id, lock_json))
    })
}

fn handle_lock_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_get()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Lock>(id)
            {
                Ok(Some(lock)) => {
                    return match serde_json::to_value(&lock) {
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

        let locks = stores.read_locks()?;
        match locks.get(id) {
            Some(lock) => match serde_json::to_value(lock) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("lock", id))),
        }
    })
}

fn handle_lock_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_list()");
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
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Lock>(&filters)
            {
                Ok(locks) => {
                    return match serde_json::to_value(&locks) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let locks = stores.read_locks()?;
        let lock_list: Vec<&Lock> = locks
            .values()
            .filter(|l| resource_filter.is_none() || Some(l.resource.as_str()) == resource_filter.as_deref())
            .filter(|l| holder_filter.is_none() || Some(l.holder_id.as_str()) == holder_filter.as_deref())
            .filter(|l| !active_only || l.is_active())
            .collect();

        match serde_json::to_value(&lock_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

fn handle_lock_release(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_release()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut locks = stores.write_locks()?;
        let lock = match locks.get_mut(&id) {
            Some(l) => l,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("lock", &id))),
        };

        if !lock.is_active() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("lock is not active"),
            ));
        }

        lock.release();
        lock.updated_at = crate::id::now_millis();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(lock.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let lock_json = match serde_json::to_value(&*lock) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("lock", &id));

        Ok(DaemonResponse::ok(req.id, lock_json))
    })
}

fn handle_lock_expire(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_lock_expire()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut locks = stores.write_locks()?;
        let lock = match locks.get_mut(&id) {
            Some(l) => l,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("lock", &id))),
        };

        if !lock.is_active() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("lock is not active"),
            ));
        }

        lock.expire();
        lock.updated_at = crate::id::now_millis();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(lock.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let lock_json = match serde_json::to_value(&*lock) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("lock", &id));

        Ok(DaemonResponse::ok(req.id, lock_json))
    })
}

// --- Worktree handlers ---

fn handle_worktree_create(
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

fn handle_worktree_list(worktree_mgr: &WorktreeManager, req: DaemonRequest) -> DaemonResponse {
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

fn handle_worktree_cleanup(
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

fn handle_worktree_refresh(worktree_mgr: &WorktreeManager, req: DaemonRequest) -> DaemonResponse {
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

/// Get the current git HEAD SHA in the given repo path.
fn get_git_head_sha(repo_path: &std::path::Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
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
    try_handler!(req.id, {
        debug!("handle_integrator_validate()");
        let tick_id = match req.params.get("tick_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("tick_id is required"),
                ));
            }
        };

        // Verify tick exists and is in Sealing state
        {
            let ticks = stores.read_ticks()?;
            let tick = match ticks.get(&tick_id) {
                Some(t) => t,
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", &tick_id))),
            };
            if tick.status != TickStatus::Sealing {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::transition_rejected(&format!(
                        "tick must be in Sealing state to validate (currently {:?})",
                        tick.status
                    )),
                ));
            }
        }

        // Transition to Validating
        {
            let mut ticks = stores.write_ticks()?;
            let tick = ticks
                .get_mut(&tick_id)
                .ok_or_else(|| eyre!("record not found: {tick_id}"))?;
            tick.status = TickStatus::Validating;
            tick.updated_at = crate::id::now_millis();

            // Persist to TaskStore if available
            if let Some(store) = &stores.store
                && let Err(e) = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .update(tick.clone())
            {
                return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
            }
        }
        debug!("[transition] tick.{}: Sealing -> Validating by Integrator", tick_id);
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "tick",
            &tick_id,
            "Sealing",
            "Validating",
            "Integrator",
        ));

        // Emit validation.started event
        let _ = event_tx.send(DaemonEvent::validation_started(&tick_id));

        // Run validation commands
        let (all_passed, validation_log) = run_validation_commands(&integrator_config.validation_commands);

        // Emit validation.completed event
        let _ = event_tx.send(DaemonEvent::validation_completed(&tick_id, all_passed, &validation_log));

        // Transition to Published or Failed based on results
        let final_status = if all_passed { TickStatus::Published } else { TickStatus::Failed };

        let tick_json = {
            let mut ticks = stores.write_ticks()?;
            let tick = ticks
                .get_mut(&tick_id)
                .ok_or_else(|| eyre!("record not found: {tick_id}"))?;
            tick.status = final_status;
            tick.validation_log = validation_log;
            tick.updated_at = crate::id::now_millis();

            if all_passed {
                tick.integration_sha = get_git_head_sha(&stores.config.project.repo_path);
            }

            // Persist to TaskStore if available
            if let Some(store) = &stores.store
                && let Err(e) = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .update(tick.clone())
            {
                return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
            }

            match serde_json::to_value(&*tick) {
                Ok(v) => v,
                Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
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

        Ok(DaemonResponse::ok(req.id, tick_json))
    })
}

fn handle_integrator_publish(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_integrator_publish()");
        let tick_id = match req.params.get("tick_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("tick_id is required"),
                ));
            }
        };

        // Verify tick exists and determine current state
        let current_status = {
            let ticks = stores.read_ticks()?;
            match ticks.get(&tick_id) {
                Some(t) => t.status,
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", &tick_id))),
            }
        };

        // If Open, transition to Sealing first
        if current_status == TickStatus::Open {
            let mut ticks = stores.write_ticks()?;
            let tick = ticks
                .get_mut(&tick_id)
                .ok_or_else(|| eyre!("record not found: {tick_id}"))?;
            tick.status = TickStatus::Sealing;
            tick.updated_at = crate::id::now_millis();

            // Persist to TaskStore if available
            if let Some(store) = &stores.store
                && let Err(e) = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .update(tick.clone())
            {
                return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
            }

            debug!("[transition] tick.{}: Open -> Sealing by Integrator", tick_id);
            let _ = event_tx.send(DaemonEvent::transition_completed(
                "tick",
                &tick_id,
                "Open",
                "Sealing",
                "Integrator",
            ));
        } else if current_status != TickStatus::Sealing {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::transition_rejected(&format!(
                    "integrator.publish requires tick in Open or Sealing state (currently {:?})",
                    current_status
                )),
            ));
        }

        // Now delegate to validate (tick is in Sealing state)
        let validate_req = DaemonRequest::new(req.id, "integrator.validate", json!({ "tick_id": tick_id }));
        Ok(handle_integrator_validate(
            stores,
            event_tx,
            integrator_config,
            validate_req,
        ))
    })
}

/// Find the latest Published Tick (by highest tick number).
fn find_latest_published_tick(stores: &Arc<Stores>) -> Option<Tick> {
    let ticks = stores.read_ticks().ok()?;
    ticks
        .values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .cloned()
}

// --- Validator handlers ---

fn handle_validator_validate(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_validator_validate()");
        let validator = match &stores.validator {
            Some(v) => v.clone(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::internal("validator is not enabled"),
                ));
            }
        };

        let collection = match req.params.get("collection").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("collection is required"),
                ));
            }
        };

        let target_id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required")));
            }
        };

        let report = match collection.as_str() {
            "plan" | "plans" => {
                let plans = stores.read_plans()?;
                let plan = match plans.get(&target_id) {
                    Some(p) => p.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &target_id)));
                    }
                };
                drop(plans);
                validator.validate_plan(&target_id, &plan.title, &plan.description, &plan.acceptance_criteria)
            }
            "spec" | "specs" => {
                let specs = stores.read_specs()?;
                let spec = match specs.get(&target_id) {
                    Some(s) => s.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &target_id)));
                    }
                };
                drop(specs);
                // Get parent plan title for context
                let plan_title = stores
                    .read_plans()?
                    .get(&spec.plan_id)
                    .map(|p| p.title.clone())
                    .unwrap_or_default();
                validator.validate_spec(&target_id, &spec.title, &spec.description, &plan_title)
            }
            "phase" | "phases" => {
                let phases = stores.read_phases()?;
                let phase = match phases.get(&target_id) {
                    Some(p) => p.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", &target_id)));
                    }
                };
                drop(phases);
                // Get parent spec title for context
                let spec_title = stores
                    .read_specs()?
                    .get(&phase.spec_id)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                validator.validate_phase(&target_id, &phase.title, &phase.description, phase.order, &spec_title)
            }
            _ => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params(&format!("unsupported collection for validation: {}", collection)),
                ));
            }
        };

        match report {
            Ok(report) => {
                // Persist to TaskStore
                if let Some(store) = &stores.store
                    && let Err(e) = store
                        .lock()
                        .map_err(|_| eyre!("taskstore lock poisoned"))?
                        .create(report.clone())
                {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
                Ok(DaemonResponse::ok(req.id, serde_json::to_value(&report)?))
            }
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

// --- Coverage Evaluator handler ---

fn handle_coverage_evaluate(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coverage_evaluate()");
        let evaluator = match &stores.evaluator {
            Some(e) => e.clone(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::internal("coverage evaluator not enabled"),
                ));
            }
        };

        let parent_collection = match req.params.get("parent_collection").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("parent_collection is required"),
                ));
            }
        };

        let parent_id = match req.params.get("parent_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("parent_id is required"),
                ));
            }
        };

        let report = match parent_collection.as_str() {
            "plan" | "plans" => {
                let plans = stores.read_plans()?;
                let plan = match plans.get(&parent_id) {
                    Some(p) => p.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &parent_id)));
                    }
                };
                drop(plans);
                // Gather all Spec children of this Plan
                let specs = stores.read_specs()?;
                let child_specs: Vec<_> = specs.values().filter(|s| s.plan_id == parent_id).collect();
                let children_ids: Vec<String> = child_specs.iter().map(|s| s.id.clone()).collect();
                let specs_list = child_specs
                    .iter()
                    .map(|s| format!("- [{}] {}: {}", s.id, s.title, s.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                drop(specs);
                evaluator.evaluate_plan_specs(
                    &parent_id,
                    &plan.title,
                    &plan.description,
                    &plan.acceptance_criteria,
                    &specs_list,
                    children_ids,
                )
            }
            "spec" | "specs" => {
                let specs = stores.read_specs()?;
                let spec = match specs.get(&parent_id) {
                    Some(s) => s.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &parent_id)));
                    }
                };
                drop(specs);
                let plan_title = {
                    let plans = stores.read_plans()?;
                    plans.get(&spec.plan_id).map(|p| p.title.clone()).unwrap_or_default()
                };
                let phases = stores.read_phases()?;
                let child_phases: Vec<_> = phases.values().filter(|p| p.spec_id == parent_id).collect();
                let children_ids: Vec<String> = child_phases.iter().map(|p| p.id.clone()).collect();
                let phases_list = child_phases
                    .iter()
                    .map(|p| format!("- [{}] {} (order: {}): {}", p.id, p.title, p.order, p.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                drop(phases);
                evaluator.evaluate_spec_phases(
                    &parent_id,
                    &spec.title,
                    &spec.description,
                    &plan_title,
                    &phases_list,
                    children_ids,
                )
            }
            "phase" | "phases" => {
                let phases = stores.read_phases()?;
                let phase = match phases.get(&parent_id) {
                    Some(p) => p.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", &parent_id)));
                    }
                };
                drop(phases);
                let spec_title = {
                    let specs = stores.read_specs()?;
                    specs.get(&phase.spec_id).map(|s| s.title.clone()).unwrap_or_default()
                };
                let works = stores.read_works()?;
                let child_works: Vec<_> = works.values().filter(|w| w.phase_id == parent_id).collect();
                let children_ids: Vec<String> = child_works.iter().map(|w| w.id.clone()).collect();
                let works_list = child_works
                    .iter()
                    .map(|w| format!("- [{}] {}: {}", w.id, w.title, w.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                drop(works);
                let params = crate::evaluator::PhaseWorksParams {
                    id: parent_id.clone(),
                    title: phase.title,
                    description: phase.description,
                    order: phase.order,
                    spec_title,
                };
                evaluator.evaluate_phase_works(&params, &works_list, children_ids)
            }
            _ => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params(&format!(
                        "unsupported parent_collection for coverage: {}",
                        parent_collection
                    )),
                ));
            }
        };

        match report {
            Ok(report) => {
                // Persist to TaskStore
                if let Some(store) = &stores.store
                    && let Err(e) = store
                        .lock()
                        .map_err(|_| eyre!("taskstore lock poisoned"))?
                        .create(report.clone())
                {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
                // Also store in memory
                stores
                    .write_coverage_reports()?
                    .insert(report.id.clone(), report.clone());
                Ok(DaemonResponse::ok(req.id, serde_json::to_value(&report)?))
            }
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

fn handle_validator_report(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_validator_report()");
        let report_id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required")));
            }
        };

        // Read from TaskStore
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<ValidationReport>(&report_id)
            {
                Ok(Some(report)) => {
                    return Ok(DaemonResponse::ok(req.id, serde_json::to_value(&report)?));
                }
                Ok(None) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::not_found("validation_report", &report_id),
                    ));
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        Ok(DaemonResponse::err(
            req.id,
            RpcError::internal("TaskStore not available"),
        ))
    })
}

fn handle_validator_reports(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_validator_reports()");
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

            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<ValidationReport>(&filters)
            {
                Ok(reports) => Ok(DaemonResponse::ok(req.id, serde_json::to_value(&reports)?)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            }
        } else {
            Ok(DaemonResponse::ok(req.id, json!([])))
        }
    })
}

// --- Tool handlers ---

fn handle_tool_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tool_list()");
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
        Ok(DaemonResponse::ok(req.id, json!({ "tools": tools })))
    })
}

// --- Coordinator goal handlers ---

fn handle_coordinator_set_goal(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coordinator_set_goal()");
        let goal_text = match req.params.get("goal").and_then(|v| v.as_str()) {
            Some(g) if !g.trim().is_empty() => g.trim().to_string(),
            _ => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("goal is required and must be non-empty"),
                ));
            }
        };

        // Deactivate any existing active goals
        {
            let mut goals = stores.write_coordinator_goals()?;
            for existing in goals.values_mut() {
                if existing.active {
                    existing.deactivate();
                    // Persist deactivation to TaskStore
                    if let Some(store) = &stores.store {
                        let _ = store
                            .lock()
                            .map_err(|_| eyre!("taskstore lock poisoned"))?
                            .update(existing.clone());
                    }
                }
            }
        }

        // Create new active goal
        let goal = CoordinatorGoal::new(goal_text);
        let goal_json = match serde_json::to_value(&goal) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = goal.id.clone();

        // Persist to TaskStore
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(goal.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_coordinator_goals()?.insert(id.clone(), goal);
        let _ = event_tx.send(DaemonEvent::record_created("coordinator_goal", &id));

        Ok(DaemonResponse::ok(req.id, goal_json))
    })
}

fn handle_coordinator_clear_goal(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coordinator_clear_goal()");
        let mut cleared_count = 0;
        {
            let mut goals = stores.write_coordinator_goals()?;
            for existing in goals.values_mut() {
                if existing.active {
                    existing.deactivate();
                    // Persist deactivation to TaskStore
                    if let Some(store) = &stores.store {
                        let _ = store
                            .lock()
                            .map_err(|_| eyre!("taskstore lock poisoned"))?
                            .update(existing.clone());
                    }
                    let _ = event_tx.send(DaemonEvent::record_updated("coordinator_goal", &existing.id));
                    cleared_count += 1;
                }
            }
        }

        Ok(DaemonResponse::ok(req.id, json!({ "cleared": cleared_count })))
    })
}

fn handle_coordinator_get_goal(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coordinator_get_goal()");
        let goals = stores.read_coordinator_goals()?;
        let active = goals.values().find(|g| g.active);
        match active {
            Some(goal) => {
                let json = match serde_json::to_value(goal) {
                    Ok(v) => v,
                    Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                };
                Ok(DaemonResponse::ok(req.id, json))
            }
            None => Ok(DaemonResponse::ok(req.id, json!({ "active": false }))),
        }
    })
}

fn handle_coordinator_get_state(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coordinator_get_state()");
        let states = stores.read_coordinator_states()?;
        // Find the state for the active goal (or any non-terminal state)
        let active = states.values().find(|s| !s.fsm_state.is_terminal());
        match active {
            Some(state) => {
                let json = match serde_json::to_value(state) {
                    Ok(v) => v,
                    Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                };
                Ok(DaemonResponse::ok(req.id, json))
            }
            None => Ok(DaemonResponse::ok(req.id, json!({ "active": false }))),
        }
    })
}

fn handle_coordinator_reset_state(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coordinator_reset_state()");
        let mut states = stores.write_coordinator_states()?;
        let removed: Vec<String> = states.keys().cloned().collect();
        for id in &removed {
            states.remove(id);
        }
        drop(states);

        // Also remove from TaskStore
        if let Some(store_arc) = &stores.store {
            let mut store = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))?;
            for id in &removed {
                let _ = store.delete::<crate::domain::coordinator_state::CoordinatorState>(id);
            }
        }

        let _ = event_tx.send(DaemonEvent::new(
            "coordinator.state_reset",
            json!({ "message": "Coordinator state cleared" }),
        ));
        Ok(DaemonResponse::ok(
            req.id,
            json!({ "reset": true, "removed": removed.len() }),
        ))
    })
}

// --- Interview handlers ---

fn handle_coordinator_interview_respond(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coordinator_interview_respond()");
        let answer = match req.params.get("answer").and_then(|v| v.as_str()) {
            Some(a) => a.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("answer is required"),
                ));
            }
        };

        // Find the active CoordinatorState
        let mut states = stores.write_coordinator_states()?;
        let state = match states.values_mut().find(|s| !s.fsm_state.is_terminal()) {
            Some(s) => s,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::internal("no active coordinator state"),
                ));
            }
        };

        // Record the exchange (questions were sent in a previous action)
        let exchange = crate::domain::coordinator_state::InterviewExchange {
            questions: vec![], // questions were already sent via InterviewQuestion action
            answer: answer.clone(),
            timestamp: crate::id::now_millis(),
        };
        state.interview_context.push(exchange);
        state.updated_at = crate::id::now_millis();

        // Persist
        if let Some(store_arc) = &stores.store
            && let Err(e) = store_arc
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(state.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let _ = event_tx.send(DaemonEvent::new(
            "coordinator.interview_response",
            json!({ "answer": answer, "exchange_count": state.interview_context.len() }),
        ));

        Ok(DaemonResponse::ok(
            req.id,
            json!({
                "status": "received",
                "exchange_count": state.interview_context.len()
            }),
        ))
    })
}

fn handle_coordinator_accept_plan(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coordinator_accept_plan()");

        // Resolve plan_id: either from existing plan_id param, or by creating a new Plan from text
        let plan_id = if let Some(id) = req.params.get("plan_id").and_then(|v| v.as_str()) {
            // Existing plan_id takes priority
            id.to_string()
        } else if let Some(text) = req.params.get("plan").and_then(|v| v.as_str()) {
            // Create a Plan record from raw text
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("plan text is empty"),
                ));
            }
            // Extract title: first non-empty line, truncated to 120 chars
            let title = trimmed
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| {
                    let t = line.trim();
                    if t.len() > 120 { t[..120].to_string() } else { t.to_string() }
                })
                .unwrap_or_else(|| "Accepted Plan".to_string());

            let plan = Plan::new(title, trimmed.to_string(), String::new());
            let id = plan.id.clone();

            // Persist to TaskStore
            if let Some(store_arc) = &stores.store
                && let Err(e) = store_arc
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create(plan.clone())
            {
                return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
            }

            // Insert into in-memory HashMap
            stores.write_plans()?.insert(id.clone(), plan);
            let _ = event_tx.send(DaemonEvent::record_created("plan", &id));

            id
        } else {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("plan_id or plan text is required"),
            ));
        };

        // Activate the Plan (Draft → Active)
        {
            let mut plans = stores.write_plans()?;
            match plans.get_mut(&plan_id) {
                Some(plan) => {
                    plan.status = HierarchyStatus::Active;
                    plan.updated_at = crate::id::now_millis();
                    if let Some(store_arc) = &stores.store
                        && let Err(e) = store_arc
                            .lock()
                            .map_err(|_| eyre!("taskstore lock poisoned"))?
                            .update(plan.clone())
                    {
                        return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                    }
                }
                None => {
                    return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &plan_id)));
                }
            }
        }

        // Update CoordinatorState: plan_approved = true, transition to Planning
        {
            let mut states = stores.write_coordinator_states()?;
            if let Some(state) = states.values_mut().find(|s| !s.fsm_state.is_terminal()) {
                state.plan_approved = true;
                state.fsm_state = crate::domain::coordinator_state::CoordinatorFsmState::Planning;
                state.updated_at = crate::id::now_millis();
                if let Some(store_arc) = &stores.store
                    && let Err(e) = store_arc
                        .lock()
                        .map_err(|_| eyre!("taskstore lock poisoned"))?
                        .update(state.clone())
                {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let _ = event_tx.send(DaemonEvent::new(
            "coordinator.plan_accepted",
            json!({ "plan_id": plan_id }),
        ));

        Ok(DaemonResponse::ok(
            req.id,
            json!({ "accepted": true, "plan_id": plan_id }),
        ))
    })
}

fn handle_coordinator_interview_question(
    _stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coordinator_interview_question()");
        let questions = match req.params.get("questions").and_then(|v| v.as_array()) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("questions array is required"),
                ));
            }
        };

        // Emit event for TUI to display
        let _ = event_tx.send(DaemonEvent::new(
            "coordinator.interview_question",
            json!({ "questions": questions }),
        ));

        Ok(DaemonResponse::ok(
            req.id,
            json!({ "status": "questions_sent", "count": questions.len() }),
        ))
    })
}

// --- Agent handlers ---

fn handle_agent_start(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_agent_start()");
        let agent_type: AgentType = match req.params.get("agent_type") {
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(t) => t,
                Err(_) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::invalid_params(
                            "invalid agent_type (implementer|reviewer|coordinator|researcher|integrator)",
                        ),
                    ));
                }
            },
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("agent_type is required"),
                ));
            }
        };

        let work_id = req
            .params
            .get("work_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let bundle_id = req
            .params
            .get("bundle_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Validate: Implementer needs work_id, Reviewer needs bundle_id
        // Thinking plane agents (Coordinator, Researcher, Integrator) don't require either.
        match agent_type {
            AgentType::Implementer => {
                if work_id.is_none() {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::invalid_params("work_id is required for implementer agents"),
                    ));
                }
            }
            AgentType::Reviewer => {
                if bundle_id.is_none() {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::invalid_params("bundle_id is required for reviewer agents"),
                    ));
                }
            }
            AgentType::Coordinator | AgentType::Researcher | AgentType::Integrator | AgentType::Chat => {
                // These agents operate without worktrees; no target ID required at start time
            }
        }

        let target_id = req
            .params
            .get("target_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let query = req.params.get("query").and_then(|v| v.as_str()).map(|s| s.to_string());

        // max_pool enforcement: reject if active sessions of this type >= max_pool
        {
            let sessions = stores.read_agent_sessions()?;
            let active_count = sessions
                .values()
                .filter(|s| s.agent_type == agent_type && !s.status.is_terminal())
                .count();
            let max_pool = max_pool_for(agent_type, &stores.config) as usize;
            if active_count >= max_pool {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::pool_exhausted(&format!(
                        "max_pool exceeded for {}: {active_count}/{max_pool} active",
                        agent_type
                    )),
                ));
            }

            // Global agent cap: 20 total active sessions
            let total_active = sessions.values().filter(|s| !s.status.is_terminal()).count();
            if total_active >= 20 {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::pool_exhausted(&format!("global agent cap exceeded: {total_active}/20 active sessions")),
                ));
            }

            // Gap #26: Researcher dedup by target_id
            if agent_type == AgentType::Researcher
                && let Some(tid) = req.params.get("target_id").and_then(|v| v.as_str())
            {
                let has_existing = sessions.values().any(|s| {
                    s.agent_type == AgentType::Researcher
                        && !s.status.is_terminal()
                        && s.target_id.as_deref() == Some(tid)
                });
                if has_existing {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Non-terminal Researcher session already exists for target_id '{}'",
                            tid
                        )),
                    ));
                }
            }

            // Implementer dedup by work_id (mirrors Gap #26 Researcher dedup by target_id)
            if agent_type == AgentType::Implementer
                && let Some(ref wi_id) = work_id
            {
                let has_existing = sessions.values().any(|s| {
                    s.agent_type == AgentType::Implementer
                        && !s.status.is_terminal()
                        && s.work_id.as_deref() == Some(wi_id)
                });
                if has_existing {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "non-terminal Implementer session already exists for work_id '{}'",
                            wi_id
                        )),
                    ));
                }
            }
        }

        // Create agent session with model from config
        let model = match agent_type {
            AgentType::Coordinator => stores.config.agents.coordinator.role.model.clone(),
            AgentType::Implementer => stores.config.agents.implementer.model.clone(),
            AgentType::Reviewer => stores.config.agents.reviewer.model.clone(),
            AgentType::Researcher => stores.config.agents.researcher.model.clone(),
            AgentType::Integrator => "deterministic".to_string(),
            AgentType::Chat => stores.config.agents.implementer.model.clone(),
        };
        let mut session = AgentSession::new(agent_type, model);
        session.work_id = work_id;
        session.bundle_id = bundle_id;
        session.target_id = target_id;
        session.query = query;
        session.daemon_session_id = stores
            .session_dir
            .as_ref()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()));

        let session_json = match serde_json::to_value(&session) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = session.id.clone();

        // Persist to TaskStore
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(session.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_agent_sessions()?.insert(id.clone(), session);
        let _ = event_tx.send(DaemonEvent::record_created("agent_session", &id));
        debug!("[agent_status] {}: -> Starting (type={:?})", id, agent_type);
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&id, AgentStatus::Starting));

        // Spawn agent task as a Tokio background task
        let task_stores = stores.clone();
        let task_event_tx = event_tx.clone();
        let task_worktree_mgr = worktree_mgr.clone();
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            crate::agents::executor::run_agent_task(task_id, agent_type, task_stores, task_event_tx, task_worktree_mgr)
                .await;
        });

        // Store JoinHandle for graceful shutdown
        stores.lock_agent_handles()?.insert(id.clone(), handle);

        Ok(DaemonResponse::ok(req.id, session_json))
    })
}

fn handle_agent_stop(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_agent_stop()");
        let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("session_id is required"),
                ));
            }
        };

        // Check if this is a Chat session (stored in chat_sessions, not agent_sessions)
        {
            let chat_sessions = stores
                .chat_sessions
                .read()
                .map_err(|_| eyre!("chat_sessions lock poisoned"))?;
            if chat_sessions.contains_key(session_id) {
                // Abort the chat task handle if it exists
                if let Ok(mut handles) = stores.lock_agent_handles()
                    && let Some(handle) = handles.remove(session_id)
                {
                    handle.abort();
                }
                // Emit final event so TUI knows streaming stopped
                let _ = event_tx.send(DaemonEvent::new(
                    "agent.llm_output",
                    serde_json::json!(crate::agents::AgentEvent::LlmOutput {
                        session_id: session_id.to_string(),
                        chunk: String::new(),
                        is_final: true,
                    }),
                ));
                return Ok(DaemonResponse::ok(
                    req.id,
                    serde_json::json!({ "session_id": session_id, "status": "Idle" }),
                ));
            }
        }

        let mut sessions = stores.write_agent_sessions()?;
        let session = match sessions.get_mut(session_id) {
            Some(s) => s,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::not_found("agent_session", session_id),
                ));
            }
        };

        if session.status.is_terminal() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::transition_rejected(&format!("agent is already in terminal state: {}", session.status)),
            ));
        }

        if let Err(e) = session.transition_to(AgentStatus::Cancelled) {
            return Ok(DaemonResponse::err(req.id, RpcError::transition_rejected(&e)));
        }

        // Persist to TaskStore
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(session.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let session_json = match serde_json::to_value(&*session) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("agent_session", session_id));
        debug!("[agent_status] {}: -> Cancelled", session_id);
        let _ = event_tx.send(DaemonEvent::agent_status_changed(session_id, AgentStatus::Cancelled));
        Ok(DaemonResponse::ok(req.id, session_json))
    })
}

fn handle_agent_pause(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_agent_pause()");
        let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("session_id is required"),
                ));
            }
        };

        let mut sessions = stores.write_agent_sessions()?;
        let session = match sessions.get_mut(session_id) {
            Some(s) => s,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::not_found("agent_session", session_id),
                ));
            }
        };

        if session.status.is_terminal() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::transition_rejected(&format!("agent is already in terminal state: {}", session.status)),
            ));
        }

        if let Err(e) = session.transition_to(AgentStatus::Paused) {
            return Ok(DaemonResponse::err(req.id, RpcError::transition_rejected(&e)));
        }

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(session.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let session_json = match serde_json::to_value(&*session) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("agent_session", session_id));
        debug!("[agent_status] {}: -> Paused", session_id);
        let _ = event_tx.send(DaemonEvent::agent_status_changed(session_id, AgentStatus::Paused));
        Ok(DaemonResponse::ok(req.id, session_json))
    })
}

fn handle_agent_resume(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_agent_resume()");
        let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("session_id is required"),
                ));
            }
        };

        let mut sessions = stores.write_agent_sessions()?;
        let session = match sessions.get_mut(session_id) {
            Some(s) => s,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::not_found("agent_session", session_id),
                ));
            }
        };

        if let Err(e) = session.transition_to(AgentStatus::Running) {
            return Ok(DaemonResponse::err(req.id, RpcError::transition_rejected(&e)));
        }

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(session.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let session_json = match serde_json::to_value(&*session) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let _ = event_tx.send(DaemonEvent::record_updated("agent_session", session_id));
        debug!("[agent_status] {}: -> Running (resumed)", session_id);
        let _ = event_tx.send(DaemonEvent::agent_status_changed(session_id, AgentStatus::Running));
        Ok(DaemonResponse::ok(req.id, session_json))
    })
}

fn handle_agent_status(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_agent_status()");
        let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("session_id is required"),
                ));
            }
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<AgentSession>(session_id)
            {
                Ok(Some(session)) => {
                    return match serde_json::to_value(&session) {
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

        let sessions = stores.read_agent_sessions()?;
        match sessions.get(session_id) {
            Some(session) => match serde_json::to_value(session) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(
                req.id,
                RpcError::not_found("agent_session", session_id),
            )),
        }
    })
}

fn handle_agent_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_agent_list()");
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
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<AgentSession>(&filters)
            {
                Ok(sessions) => match serde_json::to_value(&sessions) {
                    Ok(v) => return Ok(DaemonResponse::ok(req.id, v)),
                    Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                },
                Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            }
        }

        // Fallback to HashMap
        let sessions = stores.read_agent_sessions()?;
        let mut result: Vec<&AgentSession> = sessions.values().collect();

        if let Some(status) = status_filter {
            result.retain(|s| s.status == status);
        }
        if let Some(agent_type) = type_filter {
            result.retain(|s| s.agent_type == agent_type);
        }

        match serde_json::to_value(&result) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

// --- Agent output handler (Gap #9) ---

fn handle_agent_output(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_agent_output()");
        let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("session_id is required"),
                ));
            }
        };
        let since = req.params.get("since").and_then(|v| v.as_u64()).unwrap_or(0);

        let events = stores.read_agent_events()?;
        let output: Vec<_> = match events.get(&session_id) {
            Some(ring) => ring.iter().skip(since as usize).collect(),
            None => Vec::new(),
        };
        Ok(DaemonResponse::ok(req.id, serde_json::to_value(&output)?))
    })
}

// --- Chat handlers ---

/// Handle chat.submit — send a user message and start/resume the Chat agentic loop.
/// Spawns a daemon-side Tokio task running run_tool_loop with per-iteration checkpointing.
fn handle_chat_submit(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_chat_submit()");
        let session_id = req
            .params
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default-chat")
            .to_string();
        let message = match req.params.get("message").and_then(|v| v.as_str()) {
            Some(m) => m.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("message is required"),
                ));
            }
        };
        let funnel_state: crate::domain::chat::FunnelState = req
            .params
            .get("funnel_state")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(crate::domain::chat::FunnelState::Chat);
        let is_draft_request = req
            .params
            .get("is_draft_request")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Lazy-create ChatHistory + append user message
        let messages = {
            let mut sessions = stores
                .chat_sessions
                .write()
                .map_err(|_| eyre::eyre!("chat_sessions lock poisoned"))?;
            let history = sessions
                .entry(session_id.clone())
                .or_insert_with(|| crate::domain::chat::ChatHistory::new(session_id.clone()));
            history.funnel_state = funnel_state;

            // Append user message
            history.messages.push(crate::tools::types::Message {
                role: "user".to_string(),
                content: vec![crate::tools::types::ContentBlock::Text { text: message.clone() }],
            });
            history.updated_at = chrono::Utc::now().timestamp_millis();

            history.messages.clone()
        };

        // Check if a chat task is already running
        {
            let handles = stores.lock_agent_handles()?;
            if handles.contains_key(&session_id) {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("Chat loop is active. Wait for completion or cancel with agent.stop."),
                ));
            }
        }

        // Create daemon-side LLM client for this chat session using ChatConfig
        let chat_config = stores.config.chat.to_role_config();
        let llm =
            match crate::agents::llm_client::AgentLlmClient::new(chat_config, session_id.clone(), event_tx.clone()) {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::internal(&format!("failed to create LLM client: {}", e)),
                    ));
                }
            };

        // Create a separate LLM client for delegate subagents (fast model)
        let delegate_config = stores.config.chat.to_delegate_role_config();
        let delegate_llm: Arc<dyn crate::tools::agentic_loop::AgenticLlm> =
            match crate::agents::llm_client::AgentLlmClient::new(
                delegate_config,
                format!("{}:delegate", session_id),
                event_tx.clone(),
            ) {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::internal(&format!("failed to create delegate LLM client: {}", e)),
                    ));
                }
            };

        let system_prompt = crate::domain::chat::system_prompt_for_chat(funnel_state, is_draft_request);
        let executor = std::sync::Arc::new(crate::tools::executor::ToolExecutor::chat_with_delegation(
            &stores.config.agents.tools,
            delegate_llm,
        ));
        let max_iterations = stores.config.chat.max_iterations;
        let cwd = stores.config.project.repo_path.clone();
        let ctx = crate::tools::context::ToolContext::new(cwd, session_id.clone()).with_sandbox(false);
        let stores_clone = stores.clone();
        let session_id_clone = session_id.clone();
        let event_tx_clone = event_tx.clone();

        // Spawn the chat task with per-iteration checkpointing
        let handle = tokio::spawn(async move {
            // Build checkpoint callback that updates ChatHistory after each iteration
            let checkpoint_stores = stores_clone.clone();
            let checkpoint_sid = session_id_clone.clone();
            let checkpoint_fn = move |msgs: &[crate::tools::types::Message]| {
                if let Ok(mut sessions) = checkpoint_stores.chat_sessions.write()
                    && let Some(history) = sessions.get_mut(&checkpoint_sid)
                {
                    history.messages = msgs.to_vec();
                    history.updated_at = chrono::Utc::now().timestamp_millis();
                }
            };

            let result = crate::tools::agentic_loop::run_tool_loop(
                llm.as_ref(),
                executor.as_ref(),
                &ctx,
                &system_prompt,
                messages,
                max_iterations,
                Some(&event_tx_clone),
                Some(&checkpoint_fn),
            )
            .await;

            // Final persist on completion
            match result {
                Ok(agentic_result) => {
                    if let Ok(mut sessions) = stores_clone.chat_sessions.write()
                        && let Some(history) = sessions.get_mut(&session_id_clone)
                    {
                        history.messages = agentic_result.messages;
                        history.updated_at = chrono::Utc::now().timestamp_millis();
                    }
                    // Emit final chunk marker
                    let _ = event_tx_clone.send(DaemonEvent::new(
                        "agent.llm_output",
                        serde_json::json!(crate::agents::AgentEvent::LlmOutput {
                            session_id: session_id_clone.clone(),
                            chunk: String::new(),
                            is_final: true,
                        }),
                    ));
                }
                Err(e) => {
                    log::error!("chat task failed: {}", e);
                    // Store error as system message
                    if let Ok(mut sessions) = stores_clone.chat_sessions.write()
                        && let Some(history) = sessions.get_mut(&session_id_clone)
                    {
                        history.messages.push(crate::tools::types::Message {
                            role: "assistant".to_string(),
                            content: vec![crate::tools::types::ContentBlock::Text {
                                text: format!("[Error: {}]", e),
                            }],
                        });
                        history.updated_at = chrono::Utc::now().timestamp_millis();
                    }
                }
            }

            // Remove handle from agent_handles (task is done)
            if let Ok(mut handles) = stores_clone.lock_agent_handles() {
                handles.remove(&session_id_clone);
            }
        });

        // Store the handle for cancellation support
        {
            let mut handles = stores.lock_agent_handles()?;
            handles.insert(session_id.clone(), handle);
        }

        info!(
            "chat.submit: session={}, message_len={}, spawned task",
            session_id,
            message.len()
        );
        Ok(DaemonResponse::ok(
            req.id,
            serde_json::json!({
                "session_id": session_id,
                "status": "Running"
            }),
        ))
    })
}

/// Handle chat.attach — subscribe to a running Chat session's event stream + rehydrate history.
/// Returns full message array + current status.
fn handle_chat_attach(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_chat_attach()");
        let session_id = req
            .params
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default-chat")
            .to_string();

        // Lazy-create if needed
        let mut sessions = stores
            .chat_sessions
            .write()
            .map_err(|_| eyre::eyre!("chat_sessions lock poisoned"))?;
        let history = sessions
            .entry(session_id.clone())
            .or_insert_with(|| crate::domain::chat::ChatHistory::new(session_id.clone()));

        Ok(DaemonResponse::ok(
            req.id,
            serde_json::json!({
                "session_id": history.session_id,
                "status": "Idle",
                "funnel_state": history.funnel_state,
                "messages": history.messages,
                "streaming": false
            }),
        ))
    })
}

/// Handle chat.history — read-only history fetch (no event subscription).
fn handle_chat_history(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_chat_history()");
        let session_id = req
            .params
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default-chat")
            .to_string();

        let sessions = stores
            .chat_sessions
            .read()
            .map_err(|_| eyre::eyre!("chat_sessions lock poisoned"))?;

        match sessions.get(&session_id) {
            Some(history) => Ok(DaemonResponse::ok(
                req.id,
                serde_json::json!({
                    "session_id": history.session_id,
                    "funnel_state": history.funnel_state,
                    "messages": history.messages,
                }),
            )),
            None => Ok(DaemonResponse::ok(
                req.id,
                serde_json::json!({
                    "session_id": session_id,
                    "funnel_state": "chat",
                    "messages": [],
                }),
            )),
        }
    })
}

// --- Update handlers (Gap #1) ---

fn handle_plan_update(
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

fn handle_spec_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_spec_update()");
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
        if let Some(desc) = req.params.get("description").and_then(|v| v.as_str()) {
            spec.description = desc.to_string();
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
        let _ = event_tx.send(DaemonEvent::record_updated("specs", &id));
        Ok(DaemonResponse::ok(req.id, spec_json))
    })
}

fn handle_phase_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_phase_update()");
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

fn handle_work_update(
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

fn handle_bundle_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_bundle_update()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut bundles = stores.write_bundles()?;
        let bundle = match bundles.get_mut(&id) {
            Some(b) => b,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("bundles", &id))),
        };

        if let Some(desc) = req.params.get("description").and_then(|v| v.as_str()) {
            bundle.description = Some(desc.to_string());
        }
        // M8: Accept both "touched_paths" and "files_changed" in update
        let paths_val = req
            .params
            .get("touched_paths")
            .or_else(|| req.params.get("files_changed"));
        if let Some(paths) = paths_val.and_then(|v| v.as_array()) {
            // Gap #22: BundleSizePolicy enforcement on update
            let policy = &stores.config.strategy.bundle_size;
            if paths.len() as u32 > policy.max_files_touched {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(&format!(
                        "Bundle touches {} files, exceeds max_files_touched={}",
                        paths.len(),
                        policy.max_files_touched
                    )),
                ));
            }
            bundle.touched_paths = paths.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        // M1: Parse claims as array (backward-compat: also accepts string)
        if let Some(claims_val) = req.params.get("claims") {
            bundle.claims = match claims_val {
                serde_json::Value::Array(arr) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                serde_json::Value::String(s) => {
                    if s.is_empty() {
                        Vec::new()
                    } else {
                        vec![s.clone()]
                    }
                }
                _ => Vec::new(),
            };
        }
        if let Some(verification) = req.params.get("verification").and_then(|v| v.as_str()) {
            bundle.verification = verification.to_string();
        }
        if let Some(locks) = req.params.get("locks_used").and_then(|v| v.as_array()) {
            bundle.locks_used = locks.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(base_tick_id) = req.params.get("base_tick_id").and_then(|v| v.as_str()) {
            bundle.base_tick_id = Some(base_tick_id.to_string());
        }
        bundle.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(bundle.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let bundle_json = serde_json::to_value(&*bundle)?;
        let _ = event_tx.send(DaemonEvent::record_updated("bundles", &id));
        Ok(DaemonResponse::ok(req.id, bundle_json))
    })
}

fn handle_tick_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tick_update()");
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut ticks = stores.write_ticks()?;
        let tick = match ticks.get_mut(&id) {
            Some(t) => t,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("ticks", &id))),
        };

        if let Some(log) = req.params.get("validation_log").and_then(|v| v.as_str()) {
            tick.validation_log = log.to_string();
        }
        if let Some(bids) = req.params.get("bundle_ids").and_then(|v| v.as_array()) {
            tick.bundle_ids = bids.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(abids) = req.params.get("attempted_bundle_ids").and_then(|v| v.as_array()) {
            tick.attempted_bundle_ids = abids.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        tick.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(tick.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let tick_json = serde_json::to_value(&*tick)?;
        let _ = event_tx.send(DaemonEvent::record_updated("ticks", &id));
        Ok(DaemonResponse::ok(req.id, tick_json))
    })
}

fn handle_learning_update(
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
    use super::*;
    use crate::test_util::TestDir;
    use serde_json::json;
    use std::path::PathBuf;

    fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    fn test_stores_with_taskstore() -> (TestDir, Arc<Stores>) {
        let dir = TestDir::new("loopr-handler-test");
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
        store.rebuild_indexes::<Work>().unwrap();
        store.rebuild_indexes::<Bundle>().unwrap();
        store.rebuild_indexes::<Tick>().unwrap();
        store.rebuild_indexes::<Learning>().unwrap();
        store.rebuild_indexes::<Lock>().unwrap();
        store.rebuild_indexes::<ValidationReport>().unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        (dir, Arc::new(stores))
    }

    /// Creates stores with TaskStore AND a validator (DocValidator placeholder via Arc).
    /// This activates the validation gate for Draft → Active transitions.
    fn test_stores_with_validator_strictness(strictness: crate::config::ValidatorStrictness) -> (TestDir, Arc<Stores>) {
        let dir = TestDir::new("loopr-handler-test");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
        let mut store = taskstore::Store::open(&dir).unwrap();
        store.rebuild_indexes::<Plan>().unwrap();
        store.rebuild_indexes::<ValidationReport>().unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        let validator_config = crate::config::ValidatorConfig {
            enabled: true,
            api_key_env: "NONEXISTENT_TEST_KEY".to_string(),
            ..crate::config::ValidatorConfig::default()
        };
        stores.validator = Some(Arc::new(crate::validator::DocValidator::new(validator_config)));
        stores.config.strategy.validator_strictness = strictness;
        (dir, Arc::new(stores))
    }

    fn test_stores_with_validator() -> (TestDir, Arc<Stores>) {
        let dir = TestDir::new("loopr-handler-test");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
        let mut store = taskstore::Store::open(&dir).unwrap();
        store.rebuild_indexes::<Plan>().unwrap();
        store.rebuild_indexes::<Spec>().unwrap();
        store.rebuild_indexes::<Phase>().unwrap();
        store.rebuild_indexes::<Work>().unwrap();
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
        (dir, Arc::new(stores))
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
        assert_eq!(result["counts"]["works"], 0);
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
        let wi = Work::new("p-1".into(), "WI".into(), "".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["counts"]["plans"], 1);
        assert_eq!(result["counts"]["works"], 1);
        assert_eq!(result["counts"]["specs"], 0);
    }

    #[test]
    fn test_dispatch_status_with_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
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
        assert_eq!(result["taskstore"]["counts"]["works"], 0);

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
        // Create first Draft Plan — succeeds
        let req1 = DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"}));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());

        // Create second Draft Plan — rejected
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
        let (_dir, stores) = test_stores_with_taskstore();
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

    // --- parent status validation tests ---

    #[test]
    fn test_spec_create_rejects_complete_plan() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        // Transition plan: Draft → Active → Complete
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
        );
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
        );

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Spec Under Complete"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("complete plan"));
    }

    #[test]
    fn test_spec_create_rejects_abandoned_plan() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        // Transition plan: Draft → Abandoned
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
        );

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Spec Under Abandoned"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("abandoned plan"));
    }

    #[test]
    fn test_spec_create_rejects_duplicate_draft() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        // Create first Draft Spec — succeeds
        let req1 = DaemonRequest::new(1, "spec.create", json!({"plan_id": plan_id, "title": "Spec A"}));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());

        // Create second Draft Spec under same Plan — rejected
        let req2 = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec B"}));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(resp2.is_error());
        assert_eq!(resp2.error.unwrap().code, -32005);
    }

    #[test]
    fn test_phase_create_rejects_duplicate_draft() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        // Create first Draft Phase — succeeds
        let req1 = DaemonRequest::new(
            1,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase A", "order": 1}),
        );
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());

        // Create second Draft Phase under same Spec — rejected
        let req2 = DaemonRequest::new(
            2,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase B", "order": 2}),
        );
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(resp2.is_error());
        assert_eq!(resp2.error.unwrap().code, -32005);
    }

    #[test]
    fn test_phase_create_rejects_complete_spec() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        // Transition spec: Draft → Active → Complete
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
        );
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
        );

        let req = DaemonRequest::new(
            2,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase Under Complete", "order": 1}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("complete spec"));
    }

    #[test]
    fn test_phase_create_rejects_abandoned_spec() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        // Transition spec: Draft → Abandoned
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
        );

        let req = DaemonRequest::new(
            2,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase Under Abandoned", "order": 1}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("abandoned spec"));
    }

    #[test]
    fn test_work_create_rejects_complete_phase() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Transition phase: Draft → Active → Complete
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
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Transition phase: Draft → Abandoned
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

    #[test]
    fn test_bundle_create_rejects_done_work() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        // Directly set work status to Done via the HashMap (bypasses transition preconditions)
        {
            let mut works = stores.works.write().unwrap();
            let work = works.get_mut(&wi_id).unwrap();
            work.status = WorkStatus::Done;
        }

        let req = DaemonRequest::new(
            2,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/late"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Done work"));
    }

    #[test]
    fn test_bundle_create_rejects_abandoned_work() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        // Transition work: Ready → Abandoned
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "work.transition",
                json!({"id": wi_id, "target_status": "Abandoned", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/abandoned"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Abandoned work"));
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        );

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
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create first spec, abandon it, then create second
        let spec_a_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec A"})),
        );
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (plan_id, spec_id_1) = create_test_spec(&stores, &tx, &wm);

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
        );

        // Create a second spec under the same plan
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
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (plan_id, spec_id_1) = create_test_spec(&stores, &tx, &wm);

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
        );

        // Create a second spec under the same plan
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
        let (_dir, stores) = test_stores_with_taskstore();
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

    // --- work handlers ---

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
    fn test_work_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

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
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
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
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
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
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
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

    #[test]
    fn test_work_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

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
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

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
        let (_plan_id, spec_id, phase_id_1) = create_test_phase(&stores, &tx, &wm);

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

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(40, "work.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by phase_id_1 — should have 1
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
        let (_plan_id, _spec_id, phase_id_1) = create_test_phase(&stores, &tx, &wm);

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

    #[test]
    fn test_work_transition_draft_to_ready() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
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

        // Already Ready — transition to InProgress (with assignee, required by precondition)
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
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

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

        // Try Ready → Done (invalid: must go through InProgress, InReview, Integrated)
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
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

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

        // Implementer cannot transition Ready → InProgress (only Coordinator can)
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

        // Already Ready via auto-promotion (acceptance_criteria present) — transition to InProgress
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

    // --- bundle handlers ---

    /// Helper: create plan + spec + phase + work and return (phase_id, work_id)
    fn create_test_work(
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
                "work.create",
                json!({"phase_id": phase_id, "title": "Parent WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (phase_id, wi_id)
    }

    /// Helper: create plan + spec + phase + work + bundle and return (work_id, bundle_id)
    fn create_test_bundle(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String) {
        let (_phase_id, wi_id) = create_test_work(stores, tx, wm);
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/test", "base_tick_id": null, "claims": "Initial claims"}),
            ),
        );
        assert!(!resp.is_error(), "bundle.create failed: {:?}", resp.error);
        let bundle_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (wi_id, bundle_id)
    }

    /// Helper: create a tick and return its id
    fn create_test_tick(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>, wm: &WorktreeManager) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        assert!(!resp.is_error(), "tick.create failed: {:?}", resp.error);
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_bundle_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
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
        assert_eq!(retrieved.unwrap().claims, vec!["Persisted bundle".to_string()]);
    }

    #[test]
    fn test_bundle_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": "tick-001",
                "claims": "Add JWT signing"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["work_id"], wi_id);
        assert_eq!(result["branch_name"], "feature/auth");
        assert_eq!(result["base_tick_id"], "tick-001");
        assert_eq!(result["claims"], serde_json::json!(["Add JWT signing"]));
        assert_eq!(result["status"], "Proposed");
        assert_eq!(stores.bundles.read().unwrap().len(), 1);
    }

    #[test]
    fn test_bundle_create_no_base_tick() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/init"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["base_tick_id"].is_null());
    }

    #[test]
    fn test_bundle_create_missing_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "bundle.create", json!({"branch_name": "feature/x"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("work_id"));
    }

    #[test]
    fn test_bundle_create_work_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "bundle.create",
            json!({"work_id": "nonexistent", "branch_name": "feature/x"}),
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let req = DaemonRequest::new(40, "bundle.create", json!({"work_id": wi_id, "claims": "stuff"}));
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        // Drain plan+spec+phase+work create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/x"}),
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/auth"}),
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
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        // Create a bundle (writes to both TaskStore and HashMap)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/ts-read"}),
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
    fn test_bundle_list_filtered_by_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id_1) = create_test_work(&stores, &tx, &wm);

        // Create a second work item under the same phase
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                31,
                "work.create",
                json!({"phase_id": _phase_id, "title": "WI 2", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
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
                json!({"work_id": wi_id_1, "branch_name": "feature/a"}),
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
                json!({"work_id": wi_id_2, "branch_name": "feature/b"}),
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
            DaemonRequest::new(51, "bundle.list", json!({"work_id": wi_id_1})),
        );
        let bundles = filtered_resp.result.unwrap();
        let arr = bundles.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["branch_name"], "feature/a");
    }

    #[test]
    fn test_bundle_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id_1) = create_test_work(&stores, &tx, &wm);

        // Create a second work item under the same phase
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                31,
                "work.create",
                json!({"phase_id": _phase_id, "title": "WI 2", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
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
                json!({"work_id": wi_id_1, "branch_name": "feature/a"}),
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
                json!({"work_id": wi_id_2, "branch_name": "feature/b"}),
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
        let filtered_req = DaemonRequest::new(51, "bundle.list", json!({"work_id": wi_id_1}));
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        // Drain plan+spec+phase+work create events
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
                json!({"work_id": wi_id, "branch_name": "feature/x"}),
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/x"}),
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/x"}),
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let _tick_id = insert_published_tick(&stores, 1);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let _old_tick_id = insert_published_tick(&stores, 1);
        let latest_tick_id = insert_published_tick(&stores, 2);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let tick_id = insert_published_tick(&stores, 1);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let _tick1_id = insert_published_tick(&stores, 1);
        let tick2_id = insert_published_tick(&stores, 2);

        // Using tick1's ID should be rejected (tick2 is latest)
        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
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
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        // Drain create events
        while rx.try_recv().is_ok() {}

        let _tick_id = insert_published_tick(&stores, 1);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": "stale-id"
            }),
        );
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "bundle.rejected_stale");
        assert_eq!(event.data["bundle_work_id"], wi_id.as_str());
        assert_eq!(event.data["base_tick_id"], "stale-id");
    }

    #[test]
    fn test_bundle_create_bootstrap_no_published_tick_no_base() {
        // Bootstrap case: no published tick, no base_tick_id → OK
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/init"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
    }

    // --- Tick handler tests ---

    #[test]
    fn test_tick_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
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
    fn test_tick_create_singleton_guard_blocks_second() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create first tick (Open)
        let resp1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        assert!(!resp1.is_error());

        // Second create should fail — non-terminal Tick exists
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(51, "tick.create", json!({"number": 2})),
        );
        assert!(resp2.is_error());
        assert!(
            resp2
                .error
                .unwrap()
                .message
                .contains("non-terminal Tick already exists")
        );
    }

    #[test]
    fn test_tick_create_singleton_guard_allows_after_terminal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create and publish first tick
        let c1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let t1_id = c1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                51,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Validating", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                53,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Published", "role": "integrator"}),
            ),
        );

        // Now creation should succeed
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(54, "tick.create", json!({"number": 2})),
        );
        assert!(!resp2.is_error());
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let ic = test_integrator_config();

        // Create first tick
        let create1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let tick1_id = create1.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition tick 1 through to Published so we can create another
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": tick1_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                53,
                "tick.transition",
                json!({"id": tick1_id, "target_status": "Validating", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                54,
                "tick.transition",
                json!({"id": tick1_id, "target_status": "Published", "role": "integrator"}),
            ),
        );

        // Now create second tick (singleton guard allows it since tick 1 is terminal)
        let create2 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(55, "tick.create", json!({"number": 2})),
        );
        let tick2_id = create2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition tick 2 to Sealing
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                56,
                "tick.transition",
                json!({"id": tick2_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(&stores, &tx, &wm, &ic, DaemonRequest::new(60, "tick.list", json!(null)));
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by Published — should have 1 (tick 1)
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(61, "tick.list", json!({"status": "Published"})),
        );
        let ticks = filtered_resp.result.unwrap();
        let arr = ticks.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["number"], 1);
    }

    #[test]
    fn test_tick_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create first tick, transition to Published, then create second
        let c1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let t1_id = c1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                53,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Validating", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                54,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Published", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(55, "tick.create", json!({"number": 2})),
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
        // Tick 1 is Published, tick 2 is Open
        let filtered_req = DaemonRequest::new(61, "tick.list", json!({"status": "Open"}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        let arr = filtered_items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
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
                    "scope": "work",
                    "content": "Always run tests"
                }),
            ),
        );
        assert!(!resp.is_error());
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_learning_create_persists_to_taskstore() {
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
                    "scope": "work",
                    "content": "Always run tests before committing"
                }),
            ),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["source_id"], "wi-123");
        assert_eq!(result["scope"], "work");
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        // Create a work-scoped learning
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
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a work-scoped learning (writes to both TaskStore and HashMap)
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_taskstore();
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
    fn test_worktree_create_missing_work_id() {
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
        assert!(resp.error.as_ref().unwrap().message.contains("work_id"));
    }

    #[test]
    fn test_worktree_create_work_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.create", json!({"work_id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("not found"));
    }

    #[test]
    fn test_worktree_create_validates_work_exists() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create a full hierarchy so work exists
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        // This will fail at the git level (nonexistent repo path) but should
        // pass the work validation
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "worktree.create", json!({"work_id": wi_id})),
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
    fn test_worktree_cleanup_missing_work_id() {
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
        assert!(resp.error.as_ref().unwrap().message.contains("work_id"));
    }

    #[test]
    fn test_worktree_cleanup_work_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.cleanup", json!({"work_id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("not found"));
    }

    #[test]
    fn test_worktree_refresh_missing_work_id() {
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
        assert!(resp.error.as_ref().unwrap().message.contains("work_id"));
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
            DaemonRequest::new(1, "worktree.refresh", json!({"work_id": "nonexistent"})),
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
        // Should get transition.completed (Sealing→Validating), validation.started,
        // validation.completed, and tick.published events
        let event1 = rx.try_recv().unwrap();
        assert_eq!(event1.event, "transition.completed");
        let event2 = rx.try_recv().unwrap();
        assert_eq!(event2.event, "validation.started");
        let event3 = rx.try_recv().unwrap();
        assert_eq!(event3.event, "validation.completed");
        assert_eq!(event3.data["success"], true);
        let event4 = rx.try_recv().unwrap();
        assert_eq!(event4.event, "tick.published");
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
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.init", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "system.init failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        let collections = result["collections"].as_array().unwrap();
        assert_eq!(collections.len(), 10);
        assert!(collections.contains(&json!("plans")));
        assert!(collections.contains(&json!("specs")));
        assert!(collections.contains(&json!("phases")));
        assert!(collections.contains(&json!("works")));
        assert!(collections.contains(&json!("bundles")));
        assert!(collections.contains(&json!("ticks")));
        assert!(collections.contains(&json!("learnings")));
        assert!(collections.contains(&json!("locks")));
        assert!(collections.contains(&json!("coordinator_goals")));
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
        let (_dir, stores) = test_stores_with_taskstore();
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
        let (_dir, stores) = test_stores_with_validator();
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
        let (_dir, stores) = test_stores_with_validator();
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
        let (_dir, stores) = test_stores_with_validator();
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

    // --- Coordinator goal tests ---

    #[test]
    fn test_coordinator_set_goal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "Build auth" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "set_goal failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["goal"], "Build auth");
        assert_eq!(result["active"], true);
        assert!(!result["id"].as_str().unwrap().is_empty());
        // Verify in stores
        let goals = stores.coordinator_goals.read().unwrap();
        assert_eq!(goals.len(), 1);
        let goal = goals.values().next().unwrap();
        assert_eq!(goal.goal, "Build auth");
        assert!(goal.active);
    }

    #[test]
    fn test_coordinator_set_goal_replaces_previous() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Set first goal
        let req1 = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "First goal" }));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());
        let first_id = resp1.result.unwrap()["id"].as_str().unwrap().to_string();

        // Set second goal — deactivates first
        let req2 = DaemonRequest::new(2, "coordinator.set_goal", json!({ "goal": "Second goal" }));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp2.is_error());

        let goals = stores.coordinator_goals.read().unwrap();
        assert_eq!(goals.len(), 2);
        // First goal deactivated
        assert!(!goals[&first_id].active);
        // Second goal active
        let active_count = goals.values().filter(|g| g.active).count();
        assert_eq!(active_count, 1);
    }

    #[test]
    fn test_coordinator_set_goal_empty_rejected() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
    }

    #[test]
    fn test_coordinator_set_goal_missing_param() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.set_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
    }

    #[test]
    fn test_coordinator_clear_goal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Set a goal first
        let req1 = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "Test goal" }));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);

        // Clear it
        let req2 = DaemonRequest::new(2, "coordinator.clear_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["cleared"], 1);

        // All goals deactivated
        let goals = stores.coordinator_goals.read().unwrap();
        assert!(goals.values().all(|g| !g.active));
    }

    #[test]
    fn test_coordinator_clear_goal_when_none() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.clear_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["cleared"], 0);
    }

    #[test]
    fn test_coordinator_get_goal_returns_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Set a goal first
        let req1 = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "Build auth" }));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        // Get it
        let req2 = DaemonRequest::new(2, "coordinator.get_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["goal"], "Build auth");
        assert_eq!(result["active"], true);
    }

    #[test]
    fn test_coordinator_get_goal_when_none() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.get_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["active"], false);
    }

    // --- Pool size enforcement tests ---

    #[test]
    fn test_max_pool_for_helper() {
        let config = crate::config::Config::default();
        assert_eq!(max_pool_for(AgentType::Implementer, &config), 6);
        assert_eq!(max_pool_for(AgentType::Reviewer, &config), 2);
        assert_eq!(max_pool_for(AgentType::Coordinator, &config), 1);
        assert_eq!(max_pool_for(AgentType::Researcher, &config), 4);
        assert_eq!(max_pool_for(AgentType::Integrator, &config), 1);
    }

    #[test]
    fn test_agent_start_max_pool_enforcement() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Add a non-terminal Coordinator session to the store (simulating already running)
        let session = crate::agents::AgentSession::new(AgentType::Coordinator, "model".to_string());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Attempt to start another Coordinator — should be rejected (max_pool = 1)
        let req = DaemonRequest::new(1, "agent.start", json!({ "agent_type": "coordinator" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error(), "expected max_pool rejection");
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32004);
        assert!(err.message.contains("max_pool exceeded"));
    }

    #[tokio::test]
    async fn test_agent_start_pool_allows_after_terminal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Add a terminal (Completed) Coordinator session — should NOT count
        let mut session = crate::agents::AgentSession::new(AgentType::Coordinator, "model".to_string());
        session.status = crate::agents::AgentStatus::Completed;
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Should be allowed — no active Coordinator sessions
        let req = DaemonRequest::new(1, "agent.start", json!({ "agent_type": "coordinator" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        // This will succeed at session creation but the spawned task may fail (no runtime).
        // We just check it wasn't rejected by max_pool.
        assert!(!resp.is_error(), "expected success, got: {:?}", resp.error);
    }

    // === Coverage tests: learning CRUD ===

    #[test]
    fn test_handle_learning_update() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

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
        );
        assert!(!resp.is_error(), "learning.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["content"], "Updated content");
        assert_eq!(result["resource_tags"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_handle_learning_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.update", json!({"id": "nonexistent", "content": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_learning_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.update", json!({"content": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_learning_reinforce() {
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
        );
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Reinforce it — exercises the TaskStore persist path
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.reinforce", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["reinforcements"], 1);
    }

    #[test]
    fn test_handle_learning_contradict() {
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
        );
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

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

    #[test]
    fn test_handle_learning_promote() {
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
        );
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["promoted"], true);
    }

    #[test]
    fn test_handle_learning_demote() {
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
        );
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Promote first
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        // Then demote
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.demote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["promoted"], false);
    }

    // === Coverage tests: lock creation ===

    #[test]
    fn test_lock_create_with_ttl_param() {
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
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord-1", "ttl_secs": 300}),
            ),
        );
        assert!(!resp.is_error(), "lock.create with ttl failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert!(result["expires_at"].is_number(), "should have expires_at from ttl_secs");
    }

    #[test]
    fn test_lock_create_auto_expire() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Default max_lock_ttl_minutes is 60, so auto-expire should set expires_at
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "src/lib.rs", "holder_id": "wi-2", "granted_by": "coord-1"}),
            ),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        // Without explicit ttl_secs, auto-expire from max_lock_ttl_minutes should set expires_at
        assert!(result["expires_at"].is_number(), "should have auto-expire expires_at");
    }

    #[test]
    fn test_lock_create_renewable_flag() {
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
                json!({"resource": "src/mod.rs", "holder_id": "wi-3", "granted_by": "coord-1", "renewable": true}),
            ),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["renewable"], true);
    }

    // === Coverage tests: agent lifecycle ===

    #[test]
    fn test_handle_agent_pause() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a running agent session
        let mut session = crate::agents::AgentSession::new(AgentType::Implementer, "model".to_string());
        let _ = session.transition_to(crate::agents::AgentStatus::Running);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.pause", json!({"session_id": sid})),
        );
        assert!(!resp.is_error(), "agent.pause failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["status"], "paused");
    }

    #[test]
    fn test_handle_agent_pause_missing_session() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.pause", json!({"session_id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_agent_pause_terminal_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let mut session = crate::agents::AgentSession::new(AgentType::Implementer, "model".to_string());
        let _ = session.transition_to(crate::agents::AgentStatus::Running);
        let _ = session.transition_to(crate::agents::AgentStatus::Completed);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.pause", json!({"session_id": sid})),
        );
        assert!(resp.is_error(), "should reject pause on terminal agent");
    }

    #[test]
    fn test_handle_agent_resume() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a paused agent session
        let mut session = crate::agents::AgentSession::new(AgentType::Implementer, "model".to_string());
        let _ = session.transition_to(crate::agents::AgentStatus::Running);
        let _ = session.transition_to(crate::agents::AgentStatus::Paused);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.resume", json!({"session_id": sid})),
        );
        assert!(!resp.is_error(), "agent.resume failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["status"], "running");
    }

    #[test]
    fn test_handle_agent_resume_missing_session() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.resume", json!({"session_id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_agent_output() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // No events for session — should return empty array
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.output", json!({"session_id": "sess-1"})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);

        // Add some events and query with since=0
        {
            let event = crate::agents::AgentEvent::LlmOutput {
                session_id: "sess-1".to_string(),
                chunk: "hello world".to_string(),
                is_final: false,
            };
            let mut events = stores.agent_events.write().unwrap();
            events.entry("sess-1".to_string()).or_default().push_back(event);
        }

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "agent.output", json!({"session_id": "sess-1", "since": 0})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_handle_agent_output_missing_session_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.output", json!({})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: validator handlers ===

    #[test]
    fn test_handle_validator_validate() {
        // validator.validate requires prompts::init() and an LLM API key.
        // We test the parameter validation paths instead.
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Missing collection — exercises param validation
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"id": "plan-1"})),
        );
        assert!(resp.is_error());

        // Missing id
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "validator.validate", json!({"collection": "plans"})),
        );
        assert!(resp.is_error());

        // Plan not found — exercises the plan lookup path
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "validator.validate",
                json!({"collection": "plans", "id": "nonexistent"}),
            ),
        );
        assert!(resp.is_error());

        // Unsupported collection
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "validator.validate", json!({"collection": "widgets", "id": "x"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unsupported"));
    }

    #[test]
    fn test_handle_validator_validate_no_validator() {
        let stores = test_stores(); // no validator
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"collection": "plans", "id": "x"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("not enabled"));
    }

    #[test]
    fn test_handle_validator_validate_missing_params() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Missing collection
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"id": "x"})),
        );
        assert!(resp.is_error());

        // Missing id
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "validator.validate", json!({"collection": "plans"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_validator_validate_unknown_collection() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"collection": "unknown", "id": "x"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unsupported collection"));
    }

    #[test]
    fn test_handle_validator_validate_not_found() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Plan not found
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "validator.validate",
                json!({"collection": "plans", "id": "nonexistent"}),
            ),
        );
        assert!(resp.is_error());

        // Spec not found
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "validator.validate",
                json!({"collection": "specs", "id": "nonexistent"}),
            ),
        );
        assert!(resp.is_error());

        // Phase not found
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "validator.validate",
                json!({"collection": "phases", "id": "nonexistent"}),
            ),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_validator_report() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a validation report directly in TaskStore
        let report = ValidationReport::new(
            "plans".into(),
            "plan-1".into(),
            ValidationVerdict::Pass,
            vec![],
            "All good".into(),
            "test-model".into(),
        );
        let report_id = report.id.clone();
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.report", json!({"id": report_id})),
        );
        assert!(!resp.is_error(), "validator.report failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["verdict"], "pass");
    }

    #[test]
    fn test_handle_validator_report_not_found() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.report", json!({"id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_validator_report_no_taskstore() {
        let stores = test_stores(); // no TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.report", json!({"id": "any"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("TaskStore"));
    }

    #[test]
    fn test_handle_validator_reports() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create two reports for different targets
        let report1 = ValidationReport::new(
            "plans".into(),
            "plan-1".into(),
            ValidationVerdict::Pass,
            vec![],
            "ok".into(),
            "test-model".into(),
        );
        let report2 = ValidationReport::new(
            "plans".into(),
            "plan-2".into(),
            ValidationVerdict::Fail,
            vec![],
            "bad".into(),
            "test-model".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report1).unwrap();
        stores.store.as_ref().unwrap().lock().unwrap().create(report2).unwrap();

        // List all reports
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.reports", json!({})),
        );
        assert!(!resp.is_error());
        assert!(resp.result.unwrap().as_array().unwrap().len() >= 2);

        // Filter by target_id
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "validator.reports", json!({"target_id": "plan-1"})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);

        // Filter by collection
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "validator.reports", json!({"target_collection": "plans"})),
        );
        assert!(!resp.is_error());
        assert!(resp.result.unwrap().as_array().unwrap().len() >= 2);
    }

    #[test]
    fn test_handle_validator_reports_no_taskstore() {
        let stores = test_stores(); // no TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.reports", json!({})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    // === Coverage tests: tool.list ===

    #[test]
    fn test_handle_tool_list() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tool.list", json!({})),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["tools"].is_array());
    }

    // === Coverage tests: validation gate strictness ===

    #[test]
    fn test_validation_gate_hard_fail_on_warn() {
        use crate::config::ValidatorStrictness;

        let (_dir, stores) = test_stores_with_validator_strictness(ValidatorStrictness::HardFailOnAnyAmbiguity);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan
        let plan = Plan::new("Gate Test".into(), "desc".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create a Warn report
        let report = ValidationReport::new(
            "plans".into(),
            plan_id.clone(),
            ValidationVerdict::Warn,
            vec![],
            "Ambiguous criteria".into(),
            "test-model".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Try to transition Draft → Active — should be blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        assert!(
            resp.is_error(),
            "HardFailOnAnyAmbiguity should block Draft→Active on Warn report"
        );
    }

    #[test]
    fn test_validation_gate_suggest_only_on_fail() {
        use crate::config::ValidatorStrictness;

        let (_dir, stores) = test_stores_with_validator_strictness(ValidatorStrictness::SuggestOnly);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan
        let plan = Plan::new("Gate Test".into(), "desc".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create a Fail report — SuggestOnly should NOT block
        let report = ValidationReport::new(
            "plans".into(),
            plan_id.clone(),
            ValidationVerdict::Fail,
            vec![],
            "Failed criteria".into(),
            "test-model".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Try to transition Draft → Active — should succeed under SuggestOnly
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        assert!(
            !resp.is_error(),
            "SuggestOnly should NOT block Draft→Active even on Fail report: {:?}",
            resp.error
        );
    }

    #[test]
    fn test_validation_gate_no_report_enabled() {
        use crate::config::ValidatorStrictness;

        let (_dir, stores) = test_stores_with_validator_strictness(ValidatorStrictness::HardFailOnAnyAmbiguity);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan with NO validation reports
        let plan = Plan::new("No Reports".into(), "desc".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Try to transition Draft → Active — should be blocked (no report exists)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        assert!(
            resp.is_error(),
            "should block Draft→Active when no validation report exists"
        );
    }

    #[test]
    fn test_agent_session_model_from_config() {
        use crate::config::Config;
        // Verify the config-based model lookup matches what each agent type should get
        let config = Config::default();
        let cases: Vec<(AgentType, String)> = vec![
            (AgentType::Coordinator, config.agents.coordinator.role.model.clone()),
            (AgentType::Implementer, config.agents.implementer.model.clone()),
            (AgentType::Reviewer, config.agents.reviewer.model.clone()),
            (AgentType::Researcher, config.agents.researcher.model.clone()),
            (AgentType::Integrator, "deterministic".to_string()),
        ];
        for (agent_type, expected_model) in cases {
            let model = match agent_type {
                AgentType::Coordinator => config.agents.coordinator.role.model.clone(),
                AgentType::Implementer => config.agents.implementer.model.clone(),
                AgentType::Reviewer => config.agents.reviewer.model.clone(),
                AgentType::Researcher => config.agents.researcher.model.clone(),
                AgentType::Integrator => "deterministic".to_string(),
                AgentType::Chat => config.agents.implementer.model.clone(),
            };
            assert_eq!(model, expected_model, "model mismatch for {:?}", agent_type);
        }
        // Coordinator should specifically be Opus
        assert_eq!(config.agents.coordinator.role.model, "claude-opus-4-6");
    }

    // --- Implementer dedup tests ---

    #[test]
    fn test_implementer_dedup_rejects_second_on_same_work() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Insert a non-terminal Implementer session for work_id "wi-1"
        let mut session = AgentSession::new(AgentType::Implementer, "test-model".into());
        session.work_id = Some("wi-1".to_string());
        session.transition_to(AgentStatus::Running).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Try to start another implementer for the same work_id
        let req = DaemonRequest::new(
            1,
            "agent.start",
            json!({"agent_type": "implementer", "work_id": "wi-1"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req);

        assert!(resp.is_error(), "should reject duplicate implementer");
        let err_msg = resp.error.unwrap().message;
        assert!(
            err_msg.contains("non-terminal Implementer session already exists"),
            "unexpected error: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_implementer_dedup_allows_after_terminal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Insert a terminal (Completed) Implementer session for work_id "wi-1"
        let mut session = AgentSession::new(AgentType::Implementer, "test-model".into());
        session.work_id = Some("wi-1".to_string());
        session.transition_to(AgentStatus::Running).unwrap();
        session.transition_to(AgentStatus::Completed).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Start a new implementer for the same work_id — should pass dedup (but may fail on spawn)
        let req = DaemonRequest::new(
            1,
            "agent.start",
            json!({"agent_type": "implementer", "work_id": "wi-1"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req);

        // Should NOT be a precondition_failed error (it may error for other reasons like no LLM key)
        if resp.is_error() {
            let err_msg = &resp.error.as_ref().unwrap().message;
            assert!(
                !err_msg.contains("non-terminal Implementer session already exists"),
                "should not reject after terminal session: {}",
                err_msg
            );
        }
    }

    #[tokio::test]
    async fn test_implementer_dedup_allows_different_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Insert a non-terminal Implementer session for work_id "wi-1"
        let mut session = AgentSession::new(AgentType::Implementer, "test-model".into());
        session.work_id = Some("wi-1".to_string());
        session.transition_to(AgentStatus::Running).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Start an implementer for a DIFFERENT work_id — should pass dedup
        let req = DaemonRequest::new(
            1,
            "agent.start",
            json!({"agent_type": "implementer", "work_id": "wi-2"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req);

        // Should NOT be a precondition_failed error
        if resp.is_error() {
            let err_msg = &resp.error.as_ref().unwrap().message;
            assert!(
                !err_msg.contains("non-terminal Implementer session already exists"),
                "should not reject different work_id: {}",
                err_msg
            );
        }
    }

    // === Coverage tests: plan.update ===

    #[test]
    fn test_handle_plan_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

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

    // === Coverage tests: spec.update ===

    #[test]
    fn test_handle_spec_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

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
                    "description": "New desc"
                }),
            ),
        );
        assert!(!resp.is_error(), "spec.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Spec");
    }

    #[test]
    fn test_handle_spec_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "spec.update", json!({"id": "nonexistent", "title": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_spec_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "spec.update", json!({"title": "x"})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: phase.update ===

    #[test]
    fn test_handle_phase_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

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
        );
        assert!(!resp.is_error(), "phase.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Phase");
        assert_eq!(result["order"], 5);
    }

    #[test]
    fn test_handle_phase_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "phase.update", json!({"id": "nonexistent", "title": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_phase_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "phase.update", json!({"title": "x"})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: work.update ===

    #[test]
    fn test_handle_work_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

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

    // === Coverage tests: bundle.update ===

    #[test]
    fn test_handle_bundle_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_wi_id, bundle_id) = create_test_bundle(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "bundle.update",
                json!({
                    "id": bundle_id,
                    "description": "Updated desc",
                    "verification": "tests pass",
                    "locks_used": ["lock-1"],
                    "base_tick_id": "tick-002"
                }),
            ),
        );
        assert!(!resp.is_error(), "bundle.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["description"], "Updated desc");
        assert_eq!(result["verification"], "tests pass");
        assert_eq!(result["locks_used"].as_array().unwrap().len(), 1);
        assert_eq!(result["base_tick_id"], "tick-002");
    }

    #[test]
    fn test_handle_bundle_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "bundle.update", json!({"id": "nonexistent", "description": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_bundle_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "bundle.update", json!({"description": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_bundle_update_size_policy_rejects_too_many_files() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_wi_id, bundle_id) = create_test_bundle(&stores, &tx, &wm);

        // Default max_files_touched is 8, so 9 paths should be rejected
        let too_many_paths: Vec<String> = (0..9).map(|i| format!("file_{}.rs", i)).collect();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "bundle.update",
                json!({
                    "id": bundle_id,
                    "touched_paths": too_many_paths
                }),
            ),
        );
        assert!(resp.is_error(), "expected size policy rejection but got success");
    }

    #[test]
    fn test_handle_bundle_update_claims_string_backward_compat() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_wi_id, bundle_id) = create_test_bundle(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "bundle.update",
                json!({"id": bundle_id, "claims": "single claim string"}),
            ),
        );
        assert!(!resp.is_error(), "bundle.update claims string failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        let claims = result["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0], "single claim string");
    }

    #[test]
    fn test_handle_bundle_update_claims_array() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_wi_id, bundle_id) = create_test_bundle(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "bundle.update",
                json!({"id": bundle_id, "claims": ["claim 1", "claim 2"]}),
            ),
        );
        assert!(!resp.is_error(), "bundle.update claims array failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        let claims = result["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0], "claim 1");
        assert_eq!(claims[1], "claim 2");
    }

    // === Coverage tests: tick.update ===

    #[test]
    fn test_handle_tick_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_test_tick(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "tick.update",
                json!({
                    "id": tick_id,
                    "validation_log": "All tests passed",
                    "bundle_ids": ["b-1", "b-2"],
                    "attempted_bundle_ids": ["b-1", "b-2", "b-3"]
                }),
            ),
        );
        assert!(!resp.is_error(), "tick.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["validation_log"], "All tests passed");
        assert_eq!(result["bundle_ids"].as_array().unwrap().len(), 2);
        assert_eq!(result["attempted_bundle_ids"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_handle_tick_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.update", json!({"id": "nonexistent", "validation_log": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_tick_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.update", json!({"validation_log": "x"})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: agent.status paths ===

    #[test]
    fn test_agent_status_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        // Rebuild AgentSession indexes for TaskStore
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .rebuild_indexes::<AgentSession>()
            .unwrap();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create an agent session and insert it directly into TaskStore (NOT the HashMap)
        let session = AgentSession::new(AgentType::Implementer, "test-model".into());
        let session_id = session.id.clone();
        stores.store.as_ref().unwrap().lock().unwrap().create(session).unwrap();

        // Query via agent.status — should find it in TaskStore
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.status", json!({"session_id": session_id})),
        );
        assert!(!resp.is_error(), "agent.status failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["id"], session_id);
    }

    #[test]
    fn test_agent_status_fallback_to_hashmap() {
        let stores = test_stores(); // No TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let session = AgentSession::new(AgentType::Implementer, "test-model".into());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.status", json!({"session_id": session_id})),
        );
        assert!(!resp.is_error(), "agent.status fallback failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["id"], session_id);
    }

    // --- coordinator.accept_plan tests ---

    #[test]
    fn test_accept_plan_with_plan_id() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan first
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Test Plan", "description": "desc"})),
        );
        assert!(!resp.is_error());
        let plan_id = resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        // Accept with plan_id (backward compat)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "coordinator.accept_plan", json!({"plan_id": plan_id})),
        );
        assert!(!resp.is_error(), "accept_plan failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["accepted"], true);
        assert_eq!(result["plan_id"], plan_id);

        // Verify plan is now Active
        let plans = stores.read_plans().unwrap();
        assert_eq!(plans[&plan_id].status, HierarchyStatus::Active);
    }

    #[test]
    fn test_accept_plan_with_text() {
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
                "coordinator.accept_plan",
                json!({"plan": "My Plan Title\nGoal: Build auth"}),
            ),
        );
        assert!(!resp.is_error(), "accept_plan with text failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["accepted"], true);
        let plan_id = result["plan_id"].as_str().unwrap();

        // Verify plan was created and activated
        let plans = stores.read_plans().unwrap();
        let plan = &plans[plan_id];
        assert_eq!(plan.title, "My Plan Title");
        assert_eq!(plan.description, "My Plan Title\nGoal: Build auth");
        assert_eq!(plan.status, HierarchyStatus::Active);
    }

    #[test]
    fn test_accept_plan_neither_param() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "coordinator.accept_plan", json!({})),
        );
        assert!(resp.is_error());
        assert!(
            resp.error
                .as_ref()
                .unwrap()
                .message
                .contains("plan_id or plan text is required")
        );
    }

    #[test]
    fn test_accept_plan_empty_text() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "coordinator.accept_plan", json!({"plan": "   "})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("plan text is empty"));
    }

    #[test]
    fn test_accept_plan_title_extraction() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Title from first non-empty line
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "coordinator.accept_plan",
                json!({"plan": "\n\n  Auth Module  \nDetails here"}),
            ),
        );
        assert!(!resp.is_error());
        let plan_id = resp.result.as_ref().unwrap()["plan_id"].as_str().unwrap();
        let plans = stores.read_plans().unwrap();
        assert_eq!(plans[plan_id].title, "Auth Module");
    }
}
