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

        // Activate the Plan (Draft -> Active)
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
