use std::sync::Arc;

use eyre::eyre;
use log::debug;
use tokio::sync::broadcast;

use crate::agents::{AgentSession, AgentStatus, AgentType};
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
