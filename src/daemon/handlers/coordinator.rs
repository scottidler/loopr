use std::sync::Arc;

use eyre::eyre;
use log::debug;
use serde_json::json;
use tokio::sync::broadcast;

use crate::config::{IntegratorConfig, InterviewMode};
use crate::domain::coordinator_goal::CoordinatorGoal;
use crate::domain::coordinator_state::CoordinatorState;
use crate::domain::plan::{HierarchyStatus, Plan};
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::worktree::manager::WorktreeManager;

use crate::daemon::context::Stores;

// --- Coordinator goal handlers ---

pub(super) fn handle_coordinator_set_goal(
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

pub(super) fn handle_coordinator_clear_goal(
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

pub(super) fn handle_coordinator_get_goal(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

pub(super) fn handle_coordinator_get_state(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

pub(super) fn handle_coordinator_reset_state(
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

pub(super) fn handle_coordinator_interview_respond(
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

pub(super) fn handle_coordinator_accept_plan(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!(
            "handle_coordinator_accept_plan(plan_id={:?}, plan_len={:?})",
            req.params.get("plan_id"),
            req.params.get("plan").and_then(|v| v.as_str()).map(|s| s.len()),
        );

        // Resolve plan_id: either from existing plan_id param, or by creating a new Plan from text
        let (plan_id, plan_title) = if let Some(id) = req.params.get("plan_id").and_then(|v| v.as_str()) {
            // Existing plan_id takes priority - look up the title
            let title = stores
                .read_plans()?
                .get(id)
                .map(|p| p.title.clone())
                .unwrap_or_else(|| "Accepted Plan".to_string());
            (id.to_string(), title)
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

            let plan = Plan::new(title.clone(), trimmed.to_string(), String::new());
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

            (id, title)
        } else {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("plan_id or plan text is required"),
            ));
        };

        // Set tier: explicit user override > LLM classification > default Full.
        let explicit_tier =
            req.params
                .get("tier")
                .and_then(|v| v.as_str())
                .and_then(|s| match s.to_lowercase().as_str() {
                    "brief" => Some(crate::domain::plan::Tier::Brief),
                    "full" => Some(crate::domain::plan::Tier::Full),
                    _ => None,
                });

        let tier = if let Some(t) = explicit_tier {
            log::info!("Tier set by explicit user override: {}", t);
            t
        } else {
            // LLM classification via tier-gate prompt, using config from XDG yml
            let tg = &stores.config.tier_gate;
            if !tg.enabled {
                log::info!("Tier gate disabled, defaulting to Full");
                crate::domain::plan::Tier::default()
            } else {
                let plan_for_classify = stores.read_plans()?.get(&plan_id).cloned();
                if let Some(ref plan) = plan_for_classify {
                    let validator_config = crate::config::ValidatorConfig {
                        enabled: true,
                        provider: tg.provider.clone(),
                        model: tg.model.clone(),
                        api_key_env: tg.api_key_env.clone(),
                        max_tokens: tg.max_tokens,
                        temperature: tg.temperature,
                    };
                    let client = crate::validator::client::LlmClient::with_ureq(validator_config);
                    crate::domain::plan::classify_tier(plan, &client)
                } else {
                    crate::domain::plan::Tier::default()
                }
            }
        };

        // Activate the Plan (Draft -> Active)
        {
            let mut plans = stores.write_plans()?;
            match plans.get_mut(&plan_id) {
                Some(plan) => {
                    plan.tier = tier;
                    plan.force_status(HierarchyStatus::Active);
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

        // Create CoordinatorGoal (deactivate any existing active goals first)
        {
            let mut goals = stores.write_coordinator_goals()?;
            for existing in goals.values_mut() {
                if existing.active {
                    existing.deactivate();
                    if let Some(store) = &stores.store {
                        let _ = store
                            .lock()
                            .map_err(|_| eyre!("taskstore lock poisoned"))?
                            .update(existing.clone());
                    }
                }
            }
        }

        let goal = CoordinatorGoal::new(plan_title);
        let goal_id = goal.id.clone();

        if let Some(store_arc) = &stores.store
            && let Err(e) = store_arc
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(goal.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }
        stores.write_coordinator_goals()?.insert(goal_id.clone(), goal);
        let _ = event_tx.send(DaemonEvent::record_created("coordinator_goal", &goal_id));

        // Pre-create CoordinatorState with plan_approved=true so the Coordinator
        // skips Interviewing and starts directly in Planning.
        // InterviewMode::Skip sets fsm_state=Planning + plan_approved=true.
        {
            let coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Skip);
            let cs_id = coord_state.id.clone();
            if let Some(store_arc) = &stores.store {
                let _ = store_arc
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create(coord_state.clone());
            }
            stores.write_coordinator_states()?.insert(cs_id, coord_state);
        }

        // Start the Coordinator agent (best-effort: may fail if no Tokio runtime or already running)
        let (coordinator_session_id, coordinator_already_running) = if tokio::runtime::Handle::try_current().is_ok() {
            let start_req = DaemonRequest::new(0, "agent.start", json!({ "agent_type": "coordinator" }));
            let start_resp = super::dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
            let already_running = start_resp.is_error();
            let session_id = start_resp
                .result
                .as_ref()
                .and_then(|r| r.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (session_id, already_running)
        } else {
            // No Tokio runtime (e.g. sync tests) - Goal is created, Coordinator can be started separately
            debug!("No Tokio runtime available; Coordinator agent not auto-started");
            (String::new(), false)
        };

        let _ = event_tx.send(DaemonEvent::new(
            "coordinator.plan_accepted",
            json!({ "plan_id": plan_id, "goal_id": goal_id }),
        ));

        Ok(DaemonResponse::ok(
            req.id,
            json!({
                "accepted": true,
                "plan_id": plan_id,
                "goal_id": goal_id,
                "coordinator_session_id": coordinator_session_id,
                "coordinator_already_running": coordinator_already_running,
            }),
        ))
    })
}

pub(super) fn handle_coordinator_seed_manifest(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coordinator_seed_manifest()");

        let yaml = match req.params.get("manifest").and_then(|v| v.as_str()) {
            Some(y) => y.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("manifest (YAML string) is required"),
                ));
            }
        };

        let resolved = match crate::manifest::parse_manifest(&yaml) {
            Ok(r) => r,
            Err(e) => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params(&format!("manifest parse error: {}", e)),
                ));
            }
        };

        // Create goal
        let goal = CoordinatorGoal::new(resolved.goal.clone());
        let goal_id = goal.id.clone();
        if let Some(store) = &stores.store {
            let _ = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(goal.clone());
        }
        stores.write_coordinator_goals()?.insert(goal_id.clone(), goal);

        // Activate Plan
        let mut plan = resolved.plan;
        plan.force_status(HierarchyStatus::Active);
        plan.updated_at = crate::id::now_millis();
        let plan_id = plan.id.clone();
        if let Some(store) = &stores.store {
            let _ = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(plan.clone());
        }
        stores.write_plans()?.insert(plan_id.clone(), plan);
        let _ = event_tx.send(DaemonEvent::record_created("plans", &plan_id));

        // Activate Spec
        let mut spec = resolved.spec;
        spec.force_status(HierarchyStatus::Active);
        spec.updated_at = crate::id::now_millis();
        let spec_id = spec.id.clone();
        if let Some(store) = &stores.store {
            let _ = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(spec.clone());
        }
        stores.write_specs()?.insert(spec_id.clone(), spec);
        let _ = event_tx.send(DaemonEvent::record_created("specs", &spec_id));

        // Activate Phases
        for mut phase in resolved.phases {
            phase.force_status(HierarchyStatus::Active);
            phase.updated_at = crate::id::now_millis();
            let phase_id = phase.id.clone();
            if let Some(store) = &stores.store {
                let _ = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create(phase.clone());
            }
            stores.write_phases()?.insert(phase_id.clone(), phase);
            let _ = event_tx.send(DaemonEvent::record_created("phases", &phase_id));
        }

        // Insert Works (already in Ready status from manifest parser)
        for work in resolved.works {
            let work_id = work.id.clone();
            if let Some(store) = &stores.store {
                let _ = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create(work.clone());
            }
            stores.write_works()?.insert(work_id.clone(), work);
            let _ = event_tx.send(DaemonEvent::record_created("works", &work_id));
        }

        // Create CoordinatorState with plan_approved=true, skipping interview and generation
        let coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Skip);
        let cs_id = coord_state.id.clone();
        if let Some(store) = &stores.store {
            let _ = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(coord_state.clone());
        }
        stores.write_coordinator_states()?.insert(cs_id, coord_state);

        // Start the Coordinator agent
        let start_req = DaemonRequest::new(0, "agent.start", json!({ "agent_type": "coordinator" }));
        let _ = super::dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);

        Ok(DaemonResponse::ok(
            req.id,
            json!({
                "id": goal_id,
                "plan_id": plan_id,
            }),
        ))
    })
}

pub(super) fn handle_coordinator_interview_question(
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use crate::daemon::handlers::dispatch;
    use crate::daemon::handlers::tests::{
        test_event_tx, test_integrator_config, test_stores, test_stores_with_taskstore, test_worktree_mgr,
    };
    use crate::domain::plan::HierarchyStatus;
    use crate::ipc::protocol::DaemonRequest;
    use serde_json::json;

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

        let req1 = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "First goal" }));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());
        let first_id = resp1.result.unwrap()["id"].as_str().unwrap().to_string();

        let req2 = DaemonRequest::new(2, "coordinator.set_goal", json!({ "goal": "Second goal" }));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp2.is_error());

        let goals = stores.coordinator_goals.read().unwrap();
        assert_eq!(goals.len(), 2);
        assert!(!goals[&first_id].active);
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

        let req1 = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "Test goal" }));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);

        let req2 = DaemonRequest::new(2, "coordinator.clear_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["cleared"], 1);

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
        let req1 = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "Build auth" }));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
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

    #[test]
    fn test_accept_plan_with_plan_id() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Test Plan", "description": "desc"})),
        );
        assert!(!resp.is_error());
        let plan_id = resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

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
        assert!(result["goal_id"].as_str().is_some());

        let plans = stores.read_plans().unwrap();
        assert_eq!(plans[&plan_id].status(), HierarchyStatus::Active);

        let goals = stores.read_coordinator_goals().unwrap();
        assert!(goals.values().any(|g| g.active));
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
        let goal_id = result["goal_id"].as_str().unwrap();

        let plans = stores.read_plans().unwrap();
        let plan = &plans[plan_id];
        assert_eq!(plan.title, "My Plan Title");
        assert_eq!(plan.description, "My Plan Title\nGoal: Build auth");
        assert_eq!(plan.status(), HierarchyStatus::Active);

        let goals = stores.read_coordinator_goals().unwrap();
        let goal = &goals[goal_id];
        assert!(goal.active);
        assert_eq!(goal.goal, "My Plan Title");

        let states = stores.read_coordinator_states().unwrap();
        let state = states
            .values()
            .find(|s| s.goal_id == goal_id)
            .expect("CoordinatorState should exist for the new goal");
        assert!(state.plan_approved, "plan_approved must be true after accept_plan");
        assert_eq!(
            state.fsm_state,
            crate::domain::coordinator_state::CoordinatorFsmState::Planning,
            "FSM should start in Planning after accept_plan"
        );
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
