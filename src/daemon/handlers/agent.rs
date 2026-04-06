use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::agents::{AgentKind, AgentSession, AgentStatus};
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::worktree::manager::WorktreeManager;

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

pub(super) fn handle_agent_start(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!(
            "handle_agent_start(agent_type={:?}, work_id={:?}, bundle_id={:?})",
            req.params.get("agent_type"),
            req.params.get("work_id"),
            req.params.get("bundle_id"),
        );
        let agent_type: AgentKind = match req.params.get("agent_type") {
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
            AgentKind::Implementer => {
                if work_id.is_none() {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::invalid_params("work_id is required for implementer agents"),
                    ));
                }
            }
            AgentKind::Reviewer => {
                if bundle_id.is_none() {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::invalid_params("bundle_id is required for reviewer agents"),
                    ));
                }
            }
            AgentKind::Coordinator | AgentKind::Researcher | AgentKind::Integrator | AgentKind::Chat => {
                // These agents operate without worktrees; no target ID required at start time
            }
        }

        let target_id = req
            .params
            .get("target_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let query = req.params.get("query").and_then(|v| v.as_str()).map(|s| s.to_string());

        // Create agent session with model from config (before lock, no shared state)
        let model = match agent_type {
            AgentKind::Coordinator => stores.config.agents.coordinator.role.model.clone(),
            AgentKind::Implementer => stores.config.agents.implementer.model.clone(),
            AgentKind::Reviewer => stores.config.agents.reviewer.model.clone(),
            AgentKind::Researcher => stores.config.agents.researcher.model.clone(),
            AgentKind::Integrator => "deterministic".to_string(),
            AgentKind::Chat => stores.config.agents.implementer.model.clone(),
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

        // Atomic max_pool check + session insert under a single write lock.
        // This prevents the race where two agent.start calls both read "0 active"
        // before either writes its session (caused dual-coordinator in E2E).
        {
            let mut sessions = stores.write_agent_sessions()?;
            let active_count = sessions
                .values()
                .filter(|s| s.agent_type == agent_type && !s.status().is_terminal())
                .count();
            let max_pool = super::max_pool_for(agent_type, &stores.config) as usize;
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
            let total_active = sessions.values().filter(|s| !s.status().is_terminal()).count();
            if total_active >= 20 {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::pool_exhausted(&format!("global agent cap exceeded: {total_active}/20 active sessions")),
                ));
            }

            // Gap #26: Researcher dedup by target_id
            if agent_type == AgentKind::Researcher
                && let Some(tid) = req.params.get("target_id").and_then(|v| v.as_str())
            {
                let has_existing = sessions.values().any(|s| {
                    s.agent_type == AgentKind::Researcher
                        && !s.status().is_terminal()
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
            if agent_type == AgentKind::Implementer
                && let Some(wi_id) = session.work_id.as_deref()
            {
                let has_existing = sessions.values().any(|s| {
                    s.agent_type == AgentKind::Implementer
                        && !s.status().is_terminal()
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

            // Reviewer dedup by bundle_id (Fix 7: prevent double-spawn on auto-triage race)
            if agent_type == AgentKind::Reviewer
                && let Some(bid) = session.bundle_id.as_deref()
            {
                let has_existing = sessions.values().any(|s| {
                    s.agent_type == AgentKind::Reviewer
                        && !s.status().is_terminal()
                        && s.bundle_id.as_deref() == Some(bid)
                });
                if has_existing {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "non-terminal Reviewer session already exists for bundle_id '{}'",
                            bid
                        )),
                    ));
                }
            }

            // Persist to TaskStore while holding the sessions lock (write-ordering rule:
            // TaskStore write must happen before in-memory insert, both under the same lock).
            if let Some(store) = &stores.store
                && let Err(e) = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create(session.clone())
            {
                return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
            }

            // In-memory insert only after durable write succeeds
            sessions.insert(id.clone(), session.clone());
        }

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

pub(super) fn handle_agent_stop(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_agent_stop(params={})", req.params);
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

        if session.status().is_terminal() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::transition_rejected(&format!("agent is already in terminal state: {}", session.status())),
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

pub(super) fn handle_agent_pause(
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

        if session.status().is_terminal() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::transition_rejected(&format!("agent is already in terminal state: {}", session.status())),
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

pub(super) fn handle_agent_resume(
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

pub(super) fn handle_agent_status(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

pub(super) fn handle_agent_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_agent_list()");
        let status_filter: Option<AgentStatus> = req
            .params
            .get("status")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let type_filter: Option<AgentKind> = req
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
            result.retain(|s| s.status() == status);
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

pub(super) fn handle_agent_output(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use crate::agents::{AgentKind, AgentSession, AgentStatus};
    use crate::daemon::handlers::dispatch;
    use crate::daemon::handlers::tests::{
        test_event_tx, test_integrator_config, test_stores, test_stores_with_taskstore, test_worktree_mgr,
    };
    use crate::ipc::protocol::DaemonRequest;
    use serde_json::json;

    #[tokio::test]
    async fn test_agent_start_max_pool_enforcement() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let session = crate::agents::AgentSession::new(AgentKind::Coordinator, "model".to_string());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let req = DaemonRequest::new(1, "agent.start", json!({ "agent_type": "coordinator" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
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

        let mut session = crate::agents::AgentSession::new(AgentKind::Coordinator, "model".to_string());
        session.force_status(crate::agents::AgentStatus::Completed);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let req = DaemonRequest::new(1, "agent.start", json!({ "agent_type": "coordinator" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error(), "expected success, got: {:?}", resp.error);
    }

    #[tokio::test]
    async fn test_handle_agent_pause() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let mut session = crate::agents::AgentSession::new(AgentKind::Implementer, "model".to_string());
        let _ = session.transition_to(crate::agents::AgentStatus::Running);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.pause", json!({"session_id": sid})),
        )
        .await;
        assert!(!resp.is_error(), "agent.pause failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["status"], "paused");
    }

    #[tokio::test]
    async fn test_handle_agent_pause_missing_session() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.pause", json!({"session_id": "nonexistent"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_agent_pause_terminal_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let mut session = crate::agents::AgentSession::new(AgentKind::Implementer, "model".to_string());
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
        )
        .await;
        assert!(resp.is_error(), "should reject pause on terminal agent");
    }

    #[tokio::test]
    async fn test_handle_agent_resume() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let mut session = crate::agents::AgentSession::new(AgentKind::Implementer, "model".to_string());
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
        )
        .await;
        assert!(!resp.is_error(), "agent.resume failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["status"], "running");
    }

    #[tokio::test]
    async fn test_handle_agent_resume_missing_session() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.resume", json!({"session_id": "nonexistent"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_agent_output() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.output", json!({"session_id": "sess-1"})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);

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
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_handle_agent_output_missing_session_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.output", json!({})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[test]
    fn test_agent_session_model_from_config() {
        use crate::config::Config;
        let config = Config::default();
        let cases: Vec<(AgentKind, String)> = vec![
            (AgentKind::Coordinator, config.agents.coordinator.role.model.clone()),
            (AgentKind::Implementer, config.agents.implementer.model.clone()),
            (AgentKind::Reviewer, config.agents.reviewer.model.clone()),
            (AgentKind::Researcher, config.agents.researcher.model.clone()),
            (AgentKind::Integrator, "deterministic".to_string()),
        ];
        for (agent_type, expected_model) in cases {
            let model = match agent_type {
                AgentKind::Coordinator => config.agents.coordinator.role.model.clone(),
                AgentKind::Implementer => config.agents.implementer.model.clone(),
                AgentKind::Reviewer => config.agents.reviewer.model.clone(),
                AgentKind::Researcher => config.agents.researcher.model.clone(),
                AgentKind::Integrator => "deterministic".to_string(),
                AgentKind::Chat => config.agents.implementer.model.clone(),
            };
            assert_eq!(model, expected_model, "model mismatch for {:?}", agent_type);
        }
        assert_eq!(config.agents.coordinator.role.model, "claude-opus-4-6");
    }

    #[tokio::test]
    async fn test_implementer_dedup_rejects_second_on_same_work() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".into());
        session.work_id = Some("wi-1".to_string());
        session.transition_to(AgentStatus::Running).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let req = DaemonRequest::new(
            1,
            "agent.start",
            json!({"agent_type": "implementer", "work_id": "wi-1"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req).await;

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

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".into());
        session.work_id = Some("wi-1".to_string());
        session.transition_to(AgentStatus::Running).unwrap();
        session.transition_to(AgentStatus::Completed).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let req = DaemonRequest::new(
            1,
            "agent.start",
            json!({"agent_type": "implementer", "work_id": "wi-1"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req).await;

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

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".into());
        session.work_id = Some("wi-1".to_string());
        session.transition_to(AgentStatus::Running).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let req = DaemonRequest::new(
            1,
            "agent.start",
            json!({"agent_type": "implementer", "work_id": "wi-2"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req).await;

        if resp.is_error() {
            let err_msg = &resp.error.as_ref().unwrap().message;
            assert!(
                !err_msg.contains("non-terminal Implementer session already exists"),
                "should not reject different work_id: {}",
                err_msg
            );
        }
    }

    #[tokio::test]
    async fn test_agent_status_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
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

        let session = AgentSession::new(AgentKind::Implementer, "test-model".into());
        let session_id = session.id.clone();
        stores.store.as_ref().unwrap().lock().unwrap().create(session).unwrap();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.status", json!({"session_id": session_id})),
        )
        .await;
        assert!(!resp.is_error(), "agent.status failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["id"], session_id);
    }

    #[tokio::test]
    async fn test_agent_status_fallback_to_hashmap() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let session = AgentSession::new(AgentKind::Implementer, "test-model".into());
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
        )
        .await;
        assert!(!resp.is_error(), "agent.status fallback failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["id"], session_id);
    }
}
