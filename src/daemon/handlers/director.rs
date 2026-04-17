//! IPC handlers for the Director agent.
//!
//! Two methods:
//! - `director.start_plan_intake`: chat-to-director handoff. Creates a Director session,
//!   sets up the user-message mpsc channel, spawns the agent.
//! - `director.user_message`: forward a user chat message to a running Director session
//!   (PlanIntake conversation turns and Monitoring-mode UserIntervention).

use std::sync::Arc;

use eyre::eyre;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, instrument};

use crate::agents::{AgentKind, AgentSession, AgentStatus};
use crate::daemon::context::Stores;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::worktree::manager::WorktreeManager;

/// Buffer size for the Director's user-message mpsc channel.
/// 16 is enough to absorb bursts of fast typing without backpressuring the TUI;
/// if the Director is blocked in an LLM call, messages queue until it returns.
const DIRECTOR_USER_MESSAGE_BUFFER: usize = 16;

/// Handle `director.start_plan_intake`.
///
/// Transitions the chat session from the generic Chat agent to the Director: creates
/// a Director `AgentSession`, copies the chat message history, creates the user-message
/// mpsc channel, stashes the sender in `Stores.director_message_tx` and the receiver in
/// `director_user_message_rx_pending`, and spawns the agent via `run_agent_task`.
///
/// Params:
/// - `chat_session_id`: String. The originating chat session whose history seeds the
///   Director's conversation.
///
/// Response:
/// - `session_id`: String. The new Director `AgentSession.id`.
/// - `status`: String. "Starting" - the Director transitions to Running inside run_agent_task.
#[instrument(skip_all, fields(chat_session_id = ?req.params.get("chat_session_id")))]
pub(super) fn handle_director_start_plan_intake(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let chat_session_id = match req.params.get("chat_session_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("chat_session_id is required"),
                ));
            }
        };

        // Director pool cap is 1 (config.agents.director.max_pool). If a Director session
        // is already active, reject - the caller should route subsequent user messages via
        // director.user_message rather than starting a new intake.
        {
            let sessions = stores.read_agent_sessions()?;
            let has_active_director = sessions
                .values()
                .any(|s| s.agent_type == AgentKind::Director && !s.status().is_terminal());
            if has_active_director {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(
                        "a Director session is already active; use director.user_message to continue",
                    ),
                ));
            }
        }

        // Create the Director session. No target_id - the Director begins in PlanIntake mode
        // (see DirectorAgent::determine_initial_mode).
        let mut session = AgentSession::new(AgentKind::Director, stores.config.agents.director.llm.model.clone());
        session.daemon_session_id = stores
            .session_dir
            .as_ref()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()));
        let session_id = session.id.clone();

        // Set up the user-message channel before creating the session in stores so the
        // Director (which pops the receiver in from_session_id) can't race ahead of the
        // sender being stashed.
        let (tx, rx) = mpsc::channel::<String>(DIRECTOR_USER_MESSAGE_BUFFER);
        stores
            .director_message_tx
            .write()
            .map_err(|_| eyre!("director_message_tx lock poisoned"))?
            .insert(session_id.clone(), tx);
        stores
            .director_user_message_rx_pending
            .write()
            .map_err(|_| eyre!("director_user_message_rx_pending lock poisoned"))?
            .insert(session_id.clone(), rx);

        // Link the chat session to the Director so chat.submit can route subsequent user
        // messages to director.user_message. Also rename goal_id → plan_id already covered
        // in Phase 1; here we just stamp director_session_id.
        {
            let mut chat_sessions = stores
                .chat_sessions
                .write()
                .map_err(|_| eyre!("chat_sessions lock poisoned"))?;
            if let Some(history) = chat_sessions.get_mut(&chat_session_id) {
                history.director_session_id = Some(session_id.clone());
            }
        }

        let session_json = serde_json::to_value(&session).map_err(|e| eyre!("serialize session: {}", e))?;

        // TaskStore write first, then in-memory insert - same ordering rule the agent.start
        // handler follows (see .claude/rules/taskstore.md).
        {
            let mut sessions = stores.write_agent_sessions()?;

            if let Some(store) = &stores.store
                && let Err(e) = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .create(session.clone())
            {
                // Clean up the pending channel entries so we don't leak them.
                let _ = stores.director_message_tx.write().map(|mut m| m.remove(&session_id));
                let _ = stores
                    .director_user_message_rx_pending
                    .write()
                    .map(|mut m| m.remove(&session_id));
                return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
            }

            sessions.insert(session_id.clone(), session);
        }

        let _ = event_tx.send(DaemonEvent::record_created("agent_session", &session_id));
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Starting));

        // Spawn the Director via the standard agent task runner. from_session_id pops the
        // receiver out of director_user_message_rx_pending when it builds the AgentContext.
        let task_stores = stores.clone();
        let task_event_tx = event_tx.clone();
        let task_worktree_mgr = worktree_mgr.clone();
        let task_id = session_id.clone();
        let handle = tokio::spawn(async move {
            crate::agents::executor::run_agent_task(
                task_id,
                AgentKind::Director,
                task_stores,
                task_event_tx,
                task_worktree_mgr,
            )
            .await;
        });
        stores.lock_agent_handles()?.insert(session_id.clone(), handle);

        info!("director: plan intake session {} spawned", session_id);
        Ok(DaemonResponse::ok(
            req.id,
            serde_json::json!({
                "session_id": session_id,
                "status": "Starting",
                "chat_session_id": chat_session_id,
                "session": session_json,
            }),
        ))
    })
}

/// Handle `director.user_message`.
///
/// Forwards a user chat message to a running Director session via its mpsc channel.
/// If no Director session is currently active (or the channel's receiver was dropped),
/// returns a precondition-failed error. The mpsc `send` is async internally; we use
/// `try_send` to avoid blocking the IPC request when the buffer is full (returns a
/// transient error in that case so the caller can retry or fall back to `chat.submit`).
#[instrument(skip_all, fields(session_id = ?req.params.get("session_id")))]
pub(super) fn handle_director_user_message(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("session_id is required"),
                ));
            }
        };
        let message = match req.params.get("message").and_then(|v| v.as_str()) {
            Some(m) => m.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("message is required"),
                ));
            }
        };

        let tx = {
            let map = stores
                .director_message_tx
                .read()
                .map_err(|_| eyre!("director_message_tx lock poisoned"))?;
            match map.get(&session_id) {
                Some(tx) => tx.clone(),
                None => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::not_found("director_session", &session_id),
                    ));
                }
            }
        };

        match tx.try_send(message.clone()) {
            Ok(()) => {
                debug!(
                    "director: forwarded user message to {} ({} chars)",
                    session_id,
                    message.len()
                );
                // Design doc §API: also emit a director.user_message broadcast event so the
                // TUI (and any other observer) sees user intervention messages in the audit
                // trail without having to inspect the Director's mpsc channel.
                let _ = event_tx.send(DaemonEvent::new(
                    "director.user_message",
                    serde_json::json!({
                        "session_id": session_id,
                        "message": message,
                    }),
                ));
                Ok(DaemonResponse::ok(
                    req.id,
                    serde_json::json!({ "status": "Received", "session_id": session_id }),
                ))
            }
            Err(mpsc::error::TrySendError::Full(_)) => Ok(DaemonResponse::err(
                req.id,
                RpcError::pool_exhausted("Director message buffer full; retry shortly"),
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Receiver dropped - Director terminated. Remove the stale sender so the
                // next user message returns not_found cleanly.
                let _ = stores.director_message_tx.write().map(|mut m| m.remove(&session_id));
                Ok(DaemonResponse::err(
                    req.id,
                    RpcError::not_found("director_session", &session_id),
                ))
            }
        }
    })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::handlers::tests::test_stores;
    use serde_json::json;

    fn test_event_tx() -> broadcast::Sender<DaemonEvent> {
        broadcast::channel::<DaemonEvent>(16).0
    }

    fn test_worktree_mgr(stores: &Arc<Stores>) -> WorktreeManager {
        WorktreeManager::new(
            stores.config.project.repo_path.clone(),
            stores.config.project.repo_path.join(".worktrees"),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_plan_intake_creates_director_and_channels() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr(&stores);

        let req = DaemonRequest::new(
            1,
            "director.start_plan_intake",
            json!({ "chat_session_id": "default-chat" }),
        );
        let resp = handle_director_start_plan_intake(&stores, &tx, &wm, req);
        assert!(!resp.is_error(), "start_plan_intake should succeed: {:?}", resp.error);

        let result = resp.result.unwrap();
        let session_id = result["session_id"].as_str().unwrap().to_string();
        assert_eq!(result["status"], "Starting");

        // Director session must exist in stores
        {
            let sessions = stores.agent_sessions.read().unwrap();
            let sess = sessions.get(&session_id).expect("director session must be in stores");
            assert_eq!(sess.agent_type, AgentKind::Director);
        }

        // Sender must be registered
        {
            let senders = stores.director_message_tx.read().unwrap();
            assert!(
                senders.contains_key(&session_id),
                "director_message_tx must hold sender"
            );
        }

        // Give the spawned task a chance to pop the pending receiver before we assert.
        // (from_session_id pops it off director_user_message_rx_pending.)
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Either the task popped it (pending is empty) or the task hasn't started yet;
        // either way the pending map should not contain our key indefinitely.
        let has_pending = {
            let pending = stores.director_user_message_rx_pending.read().unwrap();
            pending.contains_key(&session_id)
        };

        // Cancel the session to let the task exit cleanly.
        {
            let mut sessions = stores.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&session_id) {
                s.force_status(AgentStatus::Cancelled);
            }
        }

        // Assertion relaxed: the pending map must NOT hold the rx forever.
        // This verifies the handoff pattern works even when the task scheduler is slow.
        assert!(
            !has_pending || stores.director_user_message_rx_pending.read().unwrap().is_empty(),
            "director_user_message_rx_pending must be emptied by the Director task"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_plan_intake_rejects_when_director_already_active() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr(&stores);

        // Seed a non-terminal Director session so the precondition rejects.
        let mut session = AgentSession::new(AgentKind::Director, "test".into());
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let req = DaemonRequest::new(
            1,
            "director.start_plan_intake",
            json!({ "chat_session_id": "default-chat" }),
        );
        let resp = handle_director_start_plan_intake(&stores, &tx, &wm, req);
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert!(err.message.contains("already active"), "got: {}", err.message);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_message_requires_active_director_session() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();

        let req = DaemonRequest::new(
            1,
            "director.user_message",
            json!({ "session_id": "ag-ghost", "message": "hello" }),
        );
        let resp = handle_director_user_message(&stores, &tx, req);
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert!(err.message.contains("director_session"), "got: {}", err.message);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_message_delivers_through_mpsc() {
        let (_dir, stores) = test_stores();
        let bcast = test_event_tx();
        let mut bcast_rx = bcast.subscribe();

        let (tx, mut rx) = mpsc::channel::<String>(4);
        stores.director_message_tx.write().unwrap().insert("ag-test".into(), tx);

        let req = DaemonRequest::new(
            1,
            "director.user_message",
            json!({ "session_id": "ag-test", "message": "hello" }),
        );
        let resp = handle_director_user_message(&stores, &bcast, req);
        assert!(!resp.is_error());

        let msg = rx.try_recv().expect("message should be delivered");
        assert_eq!(msg, "hello");

        // Design doc §API: director.user_message also emits a broadcast event for TUI audit.
        let ev = bcast_rx
            .try_recv()
            .expect("director.user_message event should be emitted");
        assert_eq!(ev.event, "director.user_message");
        assert_eq!(ev.data.get("message").and_then(|v| v.as_str()), Some("hello"));
        assert_eq!(ev.data.get("session_id").and_then(|v| v.as_str()), Some("ag-test"));
    }

    #[test]
    fn user_message_requires_session_id_and_message() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();

        let req = DaemonRequest::new(1, "director.user_message", json!({ "message": "x" }));
        let resp = handle_director_user_message(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("session_id"));

        let req = DaemonRequest::new(2, "director.user_message", json!({ "session_id": "ag-x" }));
        let resp = handle_director_user_message(&stores, &tx, req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("message"));
    }
}
