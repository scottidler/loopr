use std::sync::Arc;

use tokio::sync::broadcast;
use tracing::{info, instrument};

use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use crate::daemon::context::Stores;

/// Build a compact orchestration status string for the Executing chat system prompt.
/// Reads Works, Bundles, and agent sessions from Stores to produce a summary.
pub(super) fn build_orchestration_status(stores: &Arc<Stores>) -> String {
    let mut status = String::with_capacity(1024);

    // Coordinator FSM state
    if let Ok(states) = stores.read_coordinator_states()
        && let Some(state) = states.values().find(|s| !s.fsm_state.is_terminal())
    {
        status.push_str(&format!("Coordinator: {:?}\n", state.fsm_state));
    }

    // Active Works
    if let Ok(works) = stores.read_works() {
        let active: Vec<_> = works
            .values()
            .filter(|w| {
                !matches!(
                    w.status(),
                    crate::domain::work::WorkStatus::Done | crate::domain::work::WorkStatus::Abandoned
                )
            })
            .collect();
        if !active.is_empty() {
            status.push_str(&format!("Works ({} active):\n", active.len()));
            for w in &active {
                status.push_str(&format!("  {} [{}] {}\n", w.id, w.status(), w.title));
            }
        }
    }

    // Recent Bundle activity
    if let Ok(bundles) = stores.read_bundles() {
        let active: Vec<_> = bundles
            .values()
            .filter(|b| {
                !matches!(
                    b.status(),
                    crate::domain::bundle::BundleStatus::Merged
                        | crate::domain::bundle::BundleStatus::Rejected
                        | crate::domain::bundle::BundleStatus::Superseded
                )
            })
            .collect();
        if !active.is_empty() {
            status.push_str(&format!("Bundles ({} in flight):\n", active.len()));
            for b in &active {
                status.push_str(&format!("  {} [{}]\n", b.id, b.status()));
            }
        }
    }

    // Running agents
    if let Ok(sessions) = stores.read_agent_sessions() {
        let running: Vec<_> = sessions.values().filter(|s| !s.status().is_terminal()).collect();
        if !running.is_empty() {
            status.push_str(&format!("Agents ({} running):\n", running.len()));
            for s in &running {
                status.push_str(&format!("  {} [{:?}] {:?}\n", s.id, s.status(), s.agent_type));
            }
        }
    }

    if status.is_empty() {
        status.push_str("No orchestration activity yet.");
    }

    status
}

/// Handle chat.submit - send a user message and start/resume the Chat agentic loop.
/// Spawns a daemon-side Tokio task running run_tool_loop with per-iteration checkpointing.
#[instrument(skip_all, fields(session_id = ?req.params.get("session_id"), funnel_state = ?req.params.get("funnel_state"), message_len = req.params.get("message").and_then(|v| v.as_str()).map(|s| s.len())))]
pub(super) fn handle_chat_submit(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
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
        let delegate_llm = match crate::agents::llm_client::AgentLlmClient::new(
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

        // Build orchestration status for Executing state system prompt
        let orch_status = if funnel_state == crate::domain::chat::FunnelState::Executing {
            Some(build_orchestration_status(stores))
        } else {
            None
        };
        let system_prompt =
            crate::domain::chat::system_prompt_for_chat(funnel_state, is_draft_request, orch_status.as_deref());
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
                    tracing::error!("chat task failed: {}", e);
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
#[instrument(skip_all)]
pub(super) fn handle_chat_attach(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
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
#[instrument(skip_all)]
pub(super) fn handle_chat_history(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
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
