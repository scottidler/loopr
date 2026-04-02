use std::io;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind, MouseEventKind};
use futures::StreamExt;
use log::info;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::time::Interval;

use crate::agents::AgentEvent;
use crate::ipc::client::IpcClient;
use crate::ipc::protocol::{DaemonEvent, IpcMessage};
use crate::tui::app::{App, ChatMessage, ChatRole, ConnectionStatus, FunnelState};

use super::ipc::{dispatch_ipc_action, event_collection, refresh_collection, try_connect};
use super::render::draw;

/// Target frame interval during streaming (~30fps). Batches SSE chunks between frames
/// to avoid rendering every 1-character delta.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Reconnect interval when disconnected from daemon.
pub const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// Chat session ID used for daemon-owned chat.
pub const CHAT_SESSION_ID: &str = "default-chat";

/// Extract LLM chunk text from a DaemonEvent, if it's an LlmOutput event for the Chat session.
pub fn extract_llm_chunk(event: &DaemonEvent) -> Option<(String, bool)> {
    if event.event != "agent.llm_output" {
        return None;
    }
    let agent_event: AgentEvent = serde_json::from_value(event.data.clone()).ok()?;
    if let AgentEvent::LlmOutput {
        session_id,
        chunk,
        is_final,
    } = agent_event
        && session_id == CHAT_SESSION_ID
    {
        return Some((chunk, is_final));
    }
    None
}

/// Extract a tool event from a DaemonEvent and convert to a ChatMessage for display.
pub fn extract_tool_event(event: &DaemonEvent) -> Option<ChatMessage> {
    match event.event.as_str() {
        "agent.tool_started" => {
            let agent_event: AgentEvent = serde_json::from_value(event.data.clone()).ok()?;
            if let AgentEvent::ToolStarted { session_id, tool } = agent_event
                && session_id == CHAT_SESSION_ID
            {
                return Some(ChatMessage {
                    role: ChatRole::ToolInvocation,
                    content: format!("⟳ tool: {tool}"),
                });
            }
            None
        }
        "agent.tool_completed" => {
            let agent_event: AgentEvent = serde_json::from_value(event.data.clone()).ok()?;
            if let AgentEvent::ToolCompleted {
                session_id,
                tool,
                exit_code,
                duration_ms,
            } = agent_event
                && session_id == CHAT_SESSION_ID
            {
                let is_error = exit_code != 0;
                let prefix = if is_error { "✗" } else { "✓" };
                return Some(ChatMessage {
                    role: ChatRole::ToolInvocation,
                    content: format!("{prefix} tool: {tool} ({duration_ms}ms)"),
                });
            }
            None
        }
        "agent.timing_info" => {
            let agent_event: AgentEvent = serde_json::from_value(event.data.clone()).ok()?;
            if let AgentEvent::TimingInfo {
                session_id,
                label,
                detail,
            } = agent_event
                && session_id == CHAT_SESSION_ID
            {
                return Some(ChatMessage {
                    role: ChatRole::ToolInvocation,
                    content: format!("⏱ {label}: {detail}"),
                });
            }
            None
        }
        _ => None,
    }
}

/// Process a daemon event for Chat streaming (LLM chunks + tool events).
pub fn handle_daemon_event(app: &mut App, event: &DaemonEvent) {
    if let Some((chunk_text, is_final)) = extract_llm_chunk(event) {
        app.chat_response_buffer.push_str(&chunk_text);
        if is_final {
            let content = std::mem::take(&mut app.chat_response_buffer);
            if !content.is_empty() {
                log::debug!("[chat] assistant: {} chars", content.len());
                app.chat_history.push(ChatMessage::assistant(content));
            }
            let elapsed = app.chat_started_at.map(|t| t.elapsed()).unwrap_or_default();
            let secs = elapsed.as_secs_f64();
            let elapsed_str = if secs < 60.0 {
                format!("{secs:.1}s")
            } else {
                let mins = secs as u64 / 60;
                let rem = secs % 60.0;
                format!("{mins}m {rem:.0}s")
            };
            app.chat_history
                .push(ChatMessage::system(format!("✓ Done ({elapsed_str})")));
            app.chat_started_at = None;
            app.chat_streaming = false;
        }
    } else if let Some(tool_msg) = extract_tool_event(event) {
        // Flush any accumulated text before showing the tool event.
        // Each agentic loop iteration produces text + tool_use blocks;
        // without flushing, text from all iterations gets concatenated.
        let content = std::mem::take(&mut app.chat_response_buffer);
        if !content.is_empty() {
            app.chat_history.push(ChatMessage::assistant(content));
        }
        app.chat_history.push(tool_msg);
    }

    // Surface orchestration events in Chat when in Executing state
    if app.funnel_state == FunnelState::Executing
        && let Some(msg) = format_orchestration_event(event)
    {
        app.chat_history.push(ChatMessage::system(msg));
    }
}

/// Format an orchestration DaemonEvent into a human-readable chat system message.
/// Returns None for events that should not be surfaced in the chat.
pub fn format_orchestration_event(event: &DaemonEvent) -> Option<String> {
    let data = &event.data;
    match event.event.as_str() {
        "record.created" => {
            let collection = data.get("collection")?.as_str()?;
            let id = data.get("id")?.as_str().unwrap_or("?");
            match collection {
                "spec" => Some(format!("Created Spec: {id}")),
                "phase" => Some(format!("Created Phase: {id}")),
                "work" => Some(format!("Created Work: {id}")),
                "bundle" => Some(format!("Bundle proposed: {id}")),
                _ => None,
            }
        }
        "record.updated" => {
            let collection = data.get("collection")?.as_str()?;
            let id = data.get("id")?.as_str().unwrap_or("?");
            match collection {
                "bundle" => {
                    let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    match status {
                        "Accepted" => Some(format!("Bundle accepted: {id}")),
                        "Rejected" => Some(format!("Bundle rejected: {id} - retrying")),
                        "Merged" => Some(format!("Bundle merged: {id}")),
                        _ => None,
                    }
                }
                "work" => {
                    let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    match status {
                        "Done" => Some(format!("Work complete: {id}")),
                        "Abandoned" => Some(format!("Work abandoned: {id}")),
                        _ => None,
                    }
                }
                "tick" => {
                    let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    if status == "Published" { Some(format!("Tick published: {id}")) } else { None }
                }
                _ => None,
            }
        }
        "agent.status_changed" => {
            let agent_type = data.get("agent_type").and_then(|v| v.as_str()).unwrap_or("");
            let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let session_id = data.get("session_id").and_then(|v| v.as_str()).unwrap_or("?");
            if status == "Running" {
                match agent_type {
                    "implementer" => Some(format!("Implementer started: {session_id}")),
                    "reviewer" => Some(format!("Reviewer started: {session_id}")),
                    "researcher" => Some(format!("Researcher started: {session_id}")),
                    _ => None,
                }
            } else if status == "Completed" || status == "Failed" {
                match agent_type {
                    "implementer" | "reviewer" | "researcher" => Some(format!("{agent_type} {status}: {session_id}")),
                    _ => None,
                }
            } else {
                None
            }
        }
        "coordinator.plan_accepted" => Some("Coordinator starting decomposition.".to_string()),
        _ => None,
    }
}

/// Process a single IPC message: dispatch events to handlers and refresh collections.
pub async fn process_ipc_message(app: &mut App, client: &mut Option<IpcClient>, msg: IpcMessage) {
    match msg {
        IpcMessage::Event(event) => {
            handle_daemon_event(app, &event);
            if let Some(collection) = event_collection(&event) {
                let collection = collection.to_string();
                if let Some(c) = client.as_mut() {
                    refresh_collection(&mut app.state, c, &collection).await;
                }
            }
        }
        IpcMessage::Response(resp) => {
            if resp.is_error()
                && let Some(err) = resp.error
            {
                app.chat_history
                    .push(ChatMessage::system(format!("Error: {}", err.message)));
                app.chat_started_at = None;
                app.chat_streaming = false;
            }
        }
    }
}

/// Main select! loop: keyboard events + IPC messages + reconnection.
/// Chat execution is daemon-side; the TUI sends chat.submit IPC and renders streamed events.
pub async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    mut client: Option<IpcClient>,
    socket_path: &Path,
) -> eyre::Result<()> {
    let mut events = EventStream::new();
    let mut reconnect_timer: Interval = tokio::time::interval(RECONNECT_INTERVAL);
    // Consume the first immediate tick so we don't reconnect on startup
    reconnect_timer.tick().await;
    let mut last_render = std::time::Instant::now();

    loop {
        // Fire-and-forget chat submit -- never blocks the event loop.
        // The daemon acks via streaming events, not a synchronous response.
        if let Some(ref submit_text) = app.pending_chat_submit.take() {
            if let Some(c) = client.as_mut() {
                let is_draft_request = submit_text == "/draft";
                let message = if is_draft_request {
                    crate::prompts::store().chat_draft.clone()
                } else {
                    submit_text.clone()
                };

                log::debug!("[chat] user: {}", submit_text);

                let params = serde_json::json!({
                    "session_id": CHAT_SESSION_ID,
                    "message": message,
                    "funnel_state": app.funnel_state,
                    "is_draft_request": is_draft_request,
                });

                match c.send("chat.submit", params).await {
                    Ok(_) => {
                        app.chat_streaming = true;
                        app.chat_started_at = Some(std::time::Instant::now());
                        app.chat_response_buffer.clear();
                    }
                    Err(e) => {
                        app.chat_history.push(ChatMessage::system(format!("IPC error: {e}")));
                    }
                }
            } else {
                app.chat_history
                    .push(ChatMessage::system("Not connected to daemon.".into()));
            }
        }

        // Throttle renders to ~30fps during streaming. Always render immediately
        // for non-streaming state (keyboard events, view switches, etc.).
        let now = std::time::Instant::now();
        if !app.chat_streaming || now.duration_since(last_render) >= FRAME_INTERVAL {
            app.frame_count = app.frame_count.wrapping_add(1);
            terminal.draw(|frame| draw(app, frame))?;
            last_render = now;
        }

        if app.should_quit {
            break;
        }

        tokio::select! {
            crossterm_event = events.next() => {
                match crossterm_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        let action = crate::tui::input::handle_key(key, app.input_mode);
                        crate::tui::input::apply_action(app, action);

                        // Dispatch any pending IPC action
                        if let (Some(ipc_action), Some(c)) = (app.pending_ipc.take(), client.as_mut()) {
                            dispatch_ipc_action(c, ipc_action).await;
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) if app.current_view == crate::tui::app::View::Chat => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                let scroll = app.chat_scroll.unwrap_or(0);
                                app.chat_scroll = Some(scroll.saturating_add(3));
                            }
                            MouseEventKind::ScrollDown => {
                                if let Some(scroll) = app.chat_scroll {
                                    if scroll <= 3 {
                                        app.chat_scroll = None;
                                    } else {
                                        app.chat_scroll = Some(scroll - 3);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            ipc_msg = async { client.as_mut().expect("guarded by is_some").recv().await }, if client.is_some() => {
                // Collect the first message + drain all immediately-available IPC
                // messages. Batching SSE chunks between frames prevents rendering
                // per-character deltas.
                let mut pending: Vec<IpcMessage> = Vec::new();
                let mut disconnected = false;
                match ipc_msg {
                    Ok(Some(msg)) => pending.push(msg),
                    Ok(None) | Err(_) => disconnected = true,
                }

                if !disconnected
                    && let Some(c) = client.as_mut()
                {
                    loop {
                        match c.try_recv() {
                            Ok(Some(msg)) => pending.push(msg),
                            Ok(None) => break,
                            Err(_) => { disconnected = true; break; }
                        }
                    }
                }

                // Process all collected messages
                for msg in pending {
                    process_ipc_message(app, &mut client, msg).await;
                }

                if disconnected {
                    info!("Lost connection to daemon, will attempt reconnection");
                    app.connection = ConnectionStatus::Disconnected;
                    client = None;
                }
            }
            // During streaming, wake up at frame intervals to render even if no IPC
            // events arrive (keeps the spinner animating).
            _ = tokio::time::sleep(FRAME_INTERVAL), if app.chat_streaming => {}
            _ = reconnect_timer.tick(), if client.is_none() => {
                if let Some((new_client, session_id)) = try_connect(socket_path).await {
                    info!("Reconnected to daemon");
                    app.connection = ConnectionStatus::Connected;
                    app.session_id = session_id;
                    client = Some(new_client);
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::protocol::DaemonEvent;

    #[test]
    fn test_extract_tool_event_started() {
        let event = DaemonEvent::agent_tool_started(CHAT_SESSION_ID, "read");
        let msg = extract_tool_event(&event).unwrap();
        assert_eq!(msg.role, ChatRole::ToolInvocation);
        assert!(msg.content.contains("read"));
        assert!(msg.content.contains("⟳"));
    }

    #[test]
    fn test_extract_tool_event_completed_success() {
        let event = DaemonEvent::agent_tool_completed(CHAT_SESSION_ID, "shell", 0, 150);
        let msg = extract_tool_event(&event).unwrap();
        assert_eq!(msg.role, ChatRole::ToolInvocation);
        assert!(msg.content.contains("✓"));
        assert!(msg.content.contains("shell"));
        assert!(msg.content.contains("150ms"));
    }

    #[test]
    fn test_extract_tool_event_completed_error() {
        let event = DaemonEvent::agent_tool_completed(CHAT_SESSION_ID, "shell", 1, 50);
        let msg = extract_tool_event(&event).unwrap();
        assert!(msg.content.contains("✗"));
    }

    #[test]
    fn test_extract_tool_event_wrong_session() {
        let event = DaemonEvent::agent_tool_started("other-session", "read");
        assert!(extract_tool_event(&event).is_none());
    }

    #[test]
    fn test_extract_tool_event_unrelated_event() {
        let event = DaemonEvent::record_created("work", "w1");
        assert!(extract_tool_event(&event).is_none());
    }

    #[test]
    fn test_extract_tool_event_timing_info() {
        let event = DaemonEvent::agent_timing_info(CHAT_SESSION_ID, "iter 0", "total=3204ms llm=2891ms tools=298ms");
        let msg = extract_tool_event(&event).unwrap();
        assert_eq!(msg.role, ChatRole::ToolInvocation);
        assert!(msg.content.contains("⏱"));
        assert!(msg.content.contains("iter 0"));
        assert!(msg.content.contains("total=3204ms"));
    }

    #[test]
    fn test_extract_tool_event_timing_info_wrong_session() {
        let event = DaemonEvent::agent_timing_info("other-session", "iter 0", "total=100ms");
        assert!(extract_tool_event(&event).is_none());
    }

    #[test]
    fn test_streaming_display_finalization_with_buffer() {
        // Simulate: response buffer has accumulated text, task completes
        let mut app = App::new();
        app.chat_streaming = true;
        app.chat_response_buffer = "Hello from tool loop".to_string();

        // Simulate what the task completion handler does
        let content = std::mem::take(&mut app.chat_response_buffer);
        if !content.is_empty() {
            app.chat_history.push(ChatMessage::assistant(content));
        }
        app.chat_streaming = false;

        assert!(!app.chat_streaming);
        assert_eq!(app.chat_history.len(), 1);
        assert_eq!(app.chat_history[0].content, "Hello from tool loop");
        assert_eq!(app.chat_history[0].role, ChatRole::Assistant);
    }

    #[test]
    fn test_streaming_display_finalization_empty_buffer_uses_result_text() {
        // Simulate: buffer is empty but AgenticResult has text
        let mut app = App::new();
        app.chat_streaming = true;
        app.chat_response_buffer.clear();

        let agentic_text = "Final answer from tool loop".to_string();

        // Simulate what the task completion handler does
        let content = std::mem::take(&mut app.chat_response_buffer);
        if !content.is_empty() {
            app.chat_history.push(ChatMessage::assistant(content));
        } else if !agentic_text.is_empty() {
            app.chat_history.push(ChatMessage::assistant(agentic_text));
        }
        app.chat_streaming = false;

        assert_eq!(app.chat_history.len(), 1);
        assert_eq!(app.chat_history[0].content, "Final answer from tool loop");
    }

    #[test]
    fn test_streaming_display_max_iterations_message() {
        // Simulate: both buffer and result text are empty, but tool calls were made
        let mut app = App::new();
        app.chat_streaming = true;
        app.chat_response_buffer.clear();

        let agentic_text = String::new();
        let tool_calls_count = 3;

        let content = std::mem::take(&mut app.chat_response_buffer);
        if !content.is_empty() {
            app.chat_history.push(ChatMessage::assistant(content));
        } else if !agentic_text.is_empty() {
            app.chat_history.push(ChatMessage::assistant(agentic_text));
        } else if tool_calls_count > 0 {
            app.chat_history.push(ChatMessage::system(
                "Tool loop reached maximum iterations without a final response.".into(),
            ));
        }
        app.chat_streaming = false;

        assert_eq!(app.chat_history.len(), 1);
        assert_eq!(app.chat_history[0].role, ChatRole::System);
        assert!(app.chat_history[0].content.contains("maximum iterations"));
    }

    #[test]
    fn test_draft_request_appends_synthetic_user_message() {
        crate::prompts::init_defaults();
        // Simulates the /draft flow with canonical_messages.
        // When canonical_messages ends with assistant, a synthetic user message is appended.
        let mut canonical = vec![
            crate::tools::types::Message {
                role: "user".to_string(),
                content: vec![crate::tools::types::ContentBlock::Text {
                    text: "I want to build a todo app".to_string(),
                }],
            },
            crate::tools::types::Message {
                role: "assistant".to_string(),
                content: vec![crate::tools::types::ContentBlock::Text {
                    text: "Great idea! What framework?".to_string(),
                }],
            },
        ];

        // Before fix: messages ends with assistant -- API would reject
        assert_eq!(canonical.last().unwrap().role, "assistant");

        // Apply the same logic as the event loop
        let is_draft_request = true;
        if canonical.last().map(|m| m.role.as_str()) != Some("user") {
            let synthetic = if is_draft_request { &crate::prompts::store().chat_draft } else { "fallback" };
            canonical.push(crate::tools::types::Message {
                role: "user".to_string(),
                content: vec![crate::tools::types::ContentBlock::Text {
                    text: synthetic.to_string(),
                }],
            });
        }

        // After fix: messages ends with user
        assert_eq!(canonical.last().unwrap().role, "user");
        match &canonical.last().unwrap().content[0] {
            crate::tools::types::ContentBlock::Text { text } => {
                assert!(text.contains("plan draft"));
            }
            _ => panic!("expected Text block"),
        }
    }

    // --- format_orchestration_event tests ---

    #[test]
    fn test_orch_event_record_created_work() {
        let event = DaemonEvent::new("record.created", serde_json::json!({"collection": "work", "id": "wk-abc"}));
        assert_eq!(
            format_orchestration_event(&event),
            Some("Created Work: wk-abc".to_string())
        );
    }

    #[test]
    fn test_orch_event_record_created_spec() {
        let event = DaemonEvent::new("record.created", serde_json::json!({"collection": "spec", "id": "sp-xyz"}));
        assert_eq!(
            format_orchestration_event(&event),
            Some("Created Spec: sp-xyz".to_string())
        );
    }

    #[test]
    fn test_orch_event_record_created_phase() {
        let event = DaemonEvent::new("record.created", serde_json::json!({"collection": "phase", "id": "ph-123"}));
        assert_eq!(
            format_orchestration_event(&event),
            Some("Created Phase: ph-123".to_string())
        );
    }

    #[test]
    fn test_orch_event_record_created_bundle() {
        let event = DaemonEvent::new("record.created", serde_json::json!({"collection": "bundle", "id": "bu-999"}));
        assert_eq!(
            format_orchestration_event(&event),
            Some("Bundle proposed: bu-999".to_string())
        );
    }

    #[test]
    fn test_orch_event_record_created_irrelevant() {
        let event = DaemonEvent::new("record.created", serde_json::json!({"collection": "learning", "id": "lr-1"}));
        assert_eq!(format_orchestration_event(&event), None);
    }

    #[test]
    fn test_orch_event_bundle_accepted() {
        let event = DaemonEvent::new(
            "record.updated",
            serde_json::json!({"collection": "bundle", "id": "bu-1", "status": "Accepted"}),
        );
        assert_eq!(
            format_orchestration_event(&event),
            Some("Bundle accepted: bu-1".to_string())
        );
    }

    #[test]
    fn test_orch_event_bundle_rejected() {
        let event = DaemonEvent::new(
            "record.updated",
            serde_json::json!({"collection": "bundle", "id": "bu-2", "status": "Rejected"}),
        );
        assert_eq!(
            format_orchestration_event(&event),
            Some("Bundle rejected: bu-2 - retrying".to_string())
        );
    }

    #[test]
    fn test_orch_event_work_done() {
        let event = DaemonEvent::new(
            "record.updated",
            serde_json::json!({"collection": "work", "id": "wk-1", "status": "Done"}),
        );
        assert_eq!(
            format_orchestration_event(&event),
            Some("Work complete: wk-1".to_string())
        );
    }

    #[test]
    fn test_orch_event_tick_published() {
        let event = DaemonEvent::new(
            "record.updated",
            serde_json::json!({"collection": "tick", "id": "tk-1", "status": "Published"}),
        );
        assert_eq!(
            format_orchestration_event(&event),
            Some("Tick published: tk-1".to_string())
        );
    }

    #[test]
    fn test_orch_event_agent_running() {
        let event = DaemonEvent::new(
            "agent.status_changed",
            serde_json::json!({"agent_type": "implementer", "status": "Running", "session_id": "sess-1"}),
        );
        assert_eq!(
            format_orchestration_event(&event),
            Some("Implementer started: sess-1".to_string())
        );
    }

    #[test]
    fn test_orch_event_agent_completed() {
        let event = DaemonEvent::new(
            "agent.status_changed",
            serde_json::json!({"agent_type": "reviewer", "status": "Completed", "session_id": "sess-2"}),
        );
        assert_eq!(
            format_orchestration_event(&event),
            Some("reviewer Completed: sess-2".to_string())
        );
    }

    #[test]
    fn test_orch_event_coordinator_status_ignored() {
        let event = DaemonEvent::new(
            "agent.status_changed",
            serde_json::json!({"agent_type": "coordinator", "status": "Running", "session_id": "sess-c"}),
        );
        assert_eq!(format_orchestration_event(&event), None);
    }

    #[test]
    fn test_orch_event_plan_accepted() {
        let event = DaemonEvent::new("coordinator.plan_accepted", serde_json::json!({"plan_id": "pl-1"}));
        assert_eq!(
            format_orchestration_event(&event),
            Some("Coordinator starting decomposition.".to_string())
        );
    }

    #[test]
    fn test_orch_event_unknown_ignored() {
        let event = DaemonEvent::new("system.shutting_down", serde_json::json!({}));
        assert_eq!(format_orchestration_event(&event), None);
    }
}
