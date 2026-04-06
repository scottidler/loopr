use std::sync::Arc;

use eyre::eyre;
use serde_json::json;
use tokio::sync::broadcast;
use tracing::debug;

use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use crate::daemon::context::Stores;

// --- Coordinator goal handlers ---

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
        tracing::trace!("handle_coordinator_get_state()");
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
    use crate::daemon::handlers::tests::{test_event_tx, test_integrator_config, test_stores, test_worktree_mgr};
    use crate::domain::coordinator_goal::CoordinatorGoal;
    use crate::ipc::protocol::DaemonRequest;
    use serde_json::json;

    #[tokio::test]
    async fn test_coordinator_get_goal_returns_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Seed goal directly without IPC
        let goal = CoordinatorGoal::new("Build auth".to_string());
        stores.coordinator_goals.write().unwrap().insert(goal.id.clone(), goal);

        let req = DaemonRequest::new(2, "coordinator.get_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["goal"], "Build auth");
        assert_eq!(result["active"], true);
    }

    #[tokio::test]
    async fn test_coordinator_get_goal_when_none() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.get_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["active"], false);
    }
}
