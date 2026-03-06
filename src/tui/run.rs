use std::io;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyEventKind, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use log::{info, warn};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use tokio::time::Interval;

use crate::agents::{AgentEvent, AgentSession};
use crate::domain::bundle::Bundle;
use crate::domain::chat::{self as chat};
use crate::domain::learning::Learning;
use crate::domain::lock::Lock;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::role::Role;
use crate::domain::spec::Spec;
use crate::domain::tick::Tick;
use crate::domain::work::Work;
use crate::ipc::client::IpcClient;
use crate::ipc::protocol::{DaemonEvent, IpcMessage};

use super::app::{
    App, AppState, ChatMessage, ChatMode, ChatRole, ConnectionStatus, FunnelState, InputMode, IpcAction, View, colors,
};
use super::input::{apply_action, handle_key};
use super::views;

/// Reconnect interval when disconnected from daemon.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// Restore the terminal to normal mode. Called on both clean exit and panic.
/// Uses raw escape sequences as a fallback to guarantee mouse tracking stops.
fn restore_terminal() {
    // Hard-disable all mouse tracking modes with raw escape sequences.
    // This must happen BEFORE leaving raw mode so the sequences are sent properly.
    let _ = io::Write::write_all(
        &mut io::stdout(),
        b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l",
    );
    let _ = io::Write::flush(&mut io::stdout());
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// Run the TUI, connecting to the daemon at the given socket path.
pub async fn run_tui(socket_path: &Path) -> eyre::Result<()> {
    // Connect to daemon
    let mut client = IpcClient::connect(socket_path)
        .await
        .map_err(|e| eyre::eyre!("Failed to connect to daemon: {e}"))?;
    let handshake_resp = client
        .handshake(crate::version())
        .await
        .map_err(|e| eyre::eyre!("Handshake failed: {e}"))?;

    // Extract session_id from handshake response
    let handshake_result = handshake_resp.result.as_ref();
    let version_match = handshake_result
        .and_then(|r| r.get("version_match"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !version_match {
        return Err(eyre::eyre!(
            "daemon version mismatch (ours={}, theirs={:?})",
            crate::version(),
            handshake_result.and_then(|r| r.get("server_version")),
        ));
    }
    let session_id = handshake_result
        .and_then(|r| r.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Install panic hook that restores the terminal before printing the panic.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));

    // Create app state
    let mut app = App::new();
    app.connection = ConnectionStatus::Connected;
    app.session_id = session_id;

    // Run event loop; capture result so we always restore the terminal
    let result = event_loop(&mut terminal, &mut app, Some(client), socket_path).await;

    restore_terminal();

    if !app.session_id.is_empty() {
        eprintln!(
            "loopr session {} ended. Run `loopr diagnose dump` for diagnostics.",
            app.session_id
        );
    }

    result
}

/// Try to connect and handshake with the daemon. Returns client + session_id.
async fn try_connect(socket_path: &Path) -> Option<(IpcClient, String)> {
    let mut client = IpcClient::connect(socket_path).await.ok()?;
    let resp = client.handshake(crate::version()).await.ok()?;
    // Reject version-mismatched daemons — stale daemon must be restarted
    let version_match = resp
        .result
        .as_ref()
        .and_then(|r| r.get("version_match"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !version_match {
        log::warn!(
            "Daemon version mismatch, refusing connection (ours={}, theirs={:?})",
            crate::version(),
            resp.result.as_ref().and_then(|r| r.get("server_version")),
        );
        return None;
    }
    let session_id = resp
        .result
        .as_ref()
        .and_then(|r| r.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some((client, session_id))
}

/// Chat session ID used for daemon-owned chat.
const CHAT_SESSION_ID: &str = "default-chat";

/// Extract LLM chunk text from a DaemonEvent, if it's an LlmOutput event for the Chat session.
fn extract_llm_chunk(event: &DaemonEvent) -> Option<(String, bool)> {
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
fn extract_tool_event(event: &DaemonEvent) -> Option<ChatMessage> {
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
        _ => None,
    }
}

/// Main select! loop: keyboard events + IPC messages + reconnection.
/// Chat execution is daemon-side; the TUI sends chat.submit IPC and renders streamed events.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    mut client: Option<IpcClient>,
    socket_path: &Path,
) -> eyre::Result<()> {
    let mut events = EventStream::new();
    let mut reconnect_timer: Interval = tokio::time::interval(RECONNECT_INTERVAL);
    // Consume the first immediate tick so we don't reconnect on startup
    reconnect_timer.tick().await;

    loop {
        // Fire-and-forget chat submit — never blocks the event loop.
        // The daemon acks via streaming events, not a synchronous response.
        if let Some(ref submit_text) = app.pending_chat_submit.take() {
            if let Some(c) = client.as_mut() {
                let is_draft_request = submit_text == "/draft";
                let message = if is_draft_request {
                    chat::DRAFT_PROMPT.to_string()
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

        app.frame_count = app.frame_count.wrapping_add(1);
        terminal.draw(|frame| draw(app, frame))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            crossterm_event = events.next() => {
                match crossterm_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        let action = handle_key(key, app.input_mode);
                        apply_action(app, action);

                        // Dispatch any pending IPC action
                        if let (Some(ipc_action), Some(c)) = (app.pending_ipc.take(), client.as_mut()) {
                            dispatch_ipc_action(c, ipc_action).await;
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) if app.current_view == View::Chat => {
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
                match ipc_msg {
                    Ok(Some(IpcMessage::Event(event))) => {
                        // Handle LLM streaming chunks and tool events for Chat
                        handle_daemon_event(app, &event);

                        // Handle record events (refresh collections)
                        if let Some(collection) = event_collection(&event) {
                            let collection = collection.to_string();
                            if let Some(c) = client.as_mut() {
                                refresh_collection(&mut app.state, c, &collection).await;
                            }
                        }
                    }
                    Ok(None) | Err(_) => {
                        info!("Lost connection to daemon, will attempt reconnection");
                        app.connection = ConnectionStatus::Disconnected;
                        client = None;
                    }
                    Ok(Some(IpcMessage::Response(resp))) => {
                        // Async response (e.g., chat.submit ack) — check for errors
                        if resp.is_error() {
                            if let Some(err) = resp.error {
                                app.chat_history.push(ChatMessage::system(format!("Error: {}", err.message)));
                                app.chat_streaming = false;
                            }
                        }
                    }
                }
            }
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

/// Process a daemon event for Chat streaming (LLM chunks + tool events).
fn handle_daemon_event(app: &mut App, event: &DaemonEvent) {
    if let Some((chunk_text, is_final)) = extract_llm_chunk(event) {
        app.chat_response_buffer.push_str(&chunk_text);
        if is_final {
            let content = std::mem::take(&mut app.chat_response_buffer);
            if !content.is_empty() {
                log::debug!("[chat] assistant: {} chars", content.len());
                app.chat_history.push(ChatMessage::assistant(content));
            }
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
}

/// Send an IPC action to the daemon.
async fn dispatch_ipc_action(client: &mut IpcClient, action: IpcAction) {
    let (method, params) = match action {
        IpcAction::SetGoal(goal) => ("coordinator.set_goal".to_string(), serde_json::json!({ "goal": goal })),
        IpcAction::PauseAgent(session_id) => (
            "agent.pause".to_string(),
            serde_json::json!({ "session_id": session_id }),
        ),
        IpcAction::ResumeAgent(session_id) => (
            "agent.resume".to_string(),
            serde_json::json!({ "session_id": session_id }),
        ),
        IpcAction::StopAgent(session_id) => (
            "agent.stop".to_string(),
            serde_json::json!({ "session_id": session_id }),
        ),
        IpcAction::NewRecord { collection } => (
            format!("{collection}.create"),
            serde_json::json!({ "title": "New Record", "description": "" }),
        ),
        IpcAction::TransitionRecord { collection, id } => (
            format!("{collection}.transition"),
            serde_json::json!({ "id": id, "target_status": "Active" }),
        ),
        IpcAction::AcceptPlan(plan_text) => (
            "coordinator.accept_plan".to_string(),
            serde_json::json!({ "plan": plan_text }),
        ),
    };
    if let Err(e) = client.request(&method, params).await {
        warn!("Failed to dispatch IPC action {method}: {e}");
    }
}

/// Refresh a collection in AppState by fetching the latest list from the daemon.
async fn refresh_collection(state: &mut AppState, client: &mut IpcClient, collection: &str) {
    let method = format!("{collection}.list");
    match client.request(&method, serde_json::json!({})).await {
        Ok((resp, _events)) => {
            if let Some(result) = resp.result {
                match collection {
                    "plan" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Plan>>(result) {
                            state.plans = items;
                        }
                    }
                    "spec" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Spec>>(result) {
                            state.specs = items;
                        }
                    }
                    "phase" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Phase>>(result) {
                            state.phases = items;
                        }
                    }
                    "work" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Work>>(result) {
                            state.works = items;
                        }
                    }
                    "bundle" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Bundle>>(result) {
                            state.bundles = items;
                        }
                    }
                    "tick" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Tick>>(result) {
                            state.ticks = items;
                        }
                    }
                    "learning" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Learning>>(result) {
                            state.learnings = items;
                        }
                    }
                    "lock" => {
                        if let Ok(items) = serde_json::from_value::<Vec<Lock>>(result) {
                            state.locks = items;
                        }
                    }
                    "agent" => {
                        if let Ok(items) = serde_json::from_value::<Vec<AgentSession>>(result) {
                            state.agent_sessions = items;
                        }
                    }
                    _ => {}
                }
            }
        }
        Err(e) => {
            warn!("Failed to refresh collection {collection}: {e}");
        }
    }
}

/// Extract collection name from a daemon event, if applicable.
fn event_collection(event: &crate::ipc::protocol::DaemonEvent) -> Option<&str> {
    match event.event.as_str() {
        "record.created" | "record.updated" | "transition.completed" => event.data["collection"].as_str(),
        "tick.published" | "tick.validation_failed" => Some("tick"),
        "bundle.rejected_stale" => Some("bundle"),
        e if e.starts_with("agent.") => Some("agent"),
        _ => None,
    }
}

/// Draw the full TUI frame: header, content area, footer, and optional help overlay.
pub fn draw(app: &App, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Content
            Constraint::Length(3), // Footer
        ])
        .split(frame.area());

    render_header(app, frame, chunks[0]);
    render_content(app, frame, chunks[1]);
    render_footer(app, frame, chunks[2]);

    if app.input_mode == InputMode::GoalInput {
        draw_goal_input(app, frame, frame.area());
    }

    if app.show_help {
        draw_help_overlay(frame, frame.area());
    }
}

/// Taskdaemon-style header: ● Loopr │ Chat|Plan · Dashboard · Works · ...
fn render_header(app: &App, frame: &mut Frame, area: Rect) {
    let connection_indicator = match app.connection {
        ConnectionStatus::Connected => Span::styled("● ", Style::default().fg(Color::Green)),
        ConnectionStatus::Disconnected => Span::styled("● ", Style::default().fg(Color::Red)),
    };

    let mut spans = vec![
        connection_indicator,
        Span::styled(
            "Loopr",
            Style::default().fg(colors::HEADER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(colors::DIM)),
    ];

    // Build tab spans
    for (i, view) in View::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(colors::DIM)));
        }

        let is_active = app.current_view == *view;

        if *view == View::Chat {
            // Show Chat|Plan with active mode highlighted
            let chat_style = if is_active && app.chat_mode == ChatMode::Chat {
                Style::default().fg(colors::HEADER).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors::DIM)
            };
            let plan_style = if is_active && app.chat_mode == ChatMode::Plan {
                Style::default().fg(colors::HEADER).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors::DIM)
            };
            spans.push(Span::styled("Chat", chat_style));
            spans.push(Span::styled("|", Style::default().fg(colors::DIM)));
            spans.push(Span::styled("Plan", plan_style));
        } else {
            let style = if is_active {
                Style::default().fg(colors::HEADER).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colors::DIM)
            };
            spans.push(Span::styled(view.to_string(), style));
        }
    }

    // Calculate the width used by the left-side tabs
    let left_width: usize = spans.iter().map(|s| s.width()).sum();
    // Inner area = total area minus 2 for borders
    let inner_width = area.width.saturating_sub(2) as usize;

    // Right-aligned: "version | session_id " (1 char buffer from border)
    let version = crate::version();
    let right_text = if !app.session_id.is_empty() {
        format!("{} | {} ", version, app.session_id)
    } else {
        format!("{} ", version)
    };
    if inner_width > left_width + right_text.len() + 1 {
        let padding = inner_width.saturating_sub(left_width + right_text.len());
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(right_text, Style::default().fg(colors::DIM)));
    }

    let header_line = Line::from(spans);
    let header = Paragraph::new(header_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

/// Delegate to the current view's render function.
fn render_content(app: &App, frame: &mut Frame, area: Rect) {
    match app.current_view {
        View::Chat => views::chat::render(app, frame, area),
        View::Dashboard => views::dashboard::render(app, frame, area),
        View::Works => views::works::render(app, frame, area),
        View::Bundles => views::bundles::render(app, frame, area),
        View::Ticks => views::ticks::render(app, frame, area),
        View::Learnings => views::learnings::render(app, frame, area),
        View::Locks => views::locks::render(app, frame, area),
        View::Agents => views::agents::render(app, frame, area),
    }
}

/// Context-sensitive footer with keybinding hints.
fn render_footer(app: &App, frame: &mut Frame, area: Rect) {
    let left_spans = match app.current_view {
        View::Chat => {
            if app.input_mode == InputMode::ChatScroll {
                vec![
                    Span::styled(
                        "[Esc]",
                        Style::default().fg(colors::KEYBIND).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" Back to input  "),
                    Span::styled(
                        "[j/k]",
                        Style::default().fg(colors::KEYBIND).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" Scroll  "),
                    Span::styled("[G]", Style::default().fg(colors::KEYBIND).add_modifier(Modifier::BOLD)),
                    Span::raw(" Bottom"),
                ]
            } else {
                let kb = |text: &'static str| {
                    Span::styled(text, Style::default().fg(colors::KEYBIND).add_modifier(Modifier::BOLD))
                };
                match app.funnel_state {
                    FunnelState::Chat => vec![
                        kb("[Enter]"),
                        Span::raw(" Send  "),
                        kb("[Shift+Enter]"),
                        Span::raw(" Newline  "),
                        kb("[Esc]"),
                        Span::raw(" Scroll  "),
                        kb("/plan"),
                        Span::raw(" Plan"),
                    ],
                    FunnelState::Interview => vec![
                        kb("[Enter]"),
                        Span::raw(" Send  "),
                        kb("/draft"),
                        Span::raw(" Build Draft  "),
                        kb("/chat"),
                        Span::raw(" Chat"),
                    ],
                    FunnelState::PlanDraft => vec![
                        kb("/accept"),
                        Span::raw(" Accept Plan  "),
                        kb("/chat"),
                        Span::raw(" Chat"),
                    ],
                    FunnelState::Executing => vec![Span::raw("Executing...")],
                }
            }
        }
        _ => {
            let actions = role_actions(app.current_role);
            vec![
                Span::styled(
                    format!("[{}] ", app.current_role),
                    Style::default().fg(colors::KEYBIND).add_modifier(Modifier::BOLD),
                ),
                Span::styled(actions.join(" | "), Style::default().fg(Color::White)),
            ]
        }
    };

    let right_spans = vec![
        Span::styled(
            "[Tab]",
            Style::default().fg(colors::KEYBIND).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Views  "),
        Span::styled("[?]", Style::default().fg(colors::KEYBIND).add_modifier(Modifier::BOLD)),
        Span::raw(" Help  "),
        Span::styled(
            "[Ctrl+c]",
            Style::default().fg(colors::KEYBIND).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" Quit"),
    ];

    // Combine left and right with spacing
    let mut all_spans = left_spans;
    all_spans.push(Span::raw("  "));
    all_spans.extend(right_spans);

    let footer_line = Line::from(all_spans);
    let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}

/// Actions available for each role, shown in the footer for non-Chat views.
pub fn role_actions(role: Role) -> Vec<&'static str> {
    match role {
        Role::Coordinator => vec!["p:Pause", "r:Resume", "x:Stop", "R:Role"],
        Role::Integrator => vec!["n:New Tick", "t:Transition", "R:Role"],
        Role::Implementer => vec!["n:New Work", "t:Transition", "R:Role"],
        Role::Reviewer => vec!["t:Transition", "R:Role"],
        Role::Researcher => vec!["R:Role"],
    }
}

/// Goal input popup shown when user presses 'g'.
fn draw_goal_input(app: &App, frame: &mut Frame, area: Rect) {
    let width = 50.min(area.width.saturating_sub(4));
    let height = 3;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup_area);

    let input_text = format!("{}_", app.goal_input);
    let input = Paragraph::new(Line::from(input_text)).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Set Goal (Enter=submit, Esc=cancel)")
            .style(Style::default().bg(Color::DarkGray)),
    );
    frame.render_widget(input, popup_area);
}

/// Centered help overlay showing keyboard shortcuts.
fn draw_help_overlay(frame: &mut Frame, area: Rect) {
    let help_text = vec![
        Line::from("Keyboard Shortcuts"),
        Line::from(""),
        Line::from("Tab        Next view"),
        Line::from("Shift+Tab  Previous view"),
        Line::from("j / Down   Select next item"),
        Line::from("k / Up     Select previous item"),
        Line::from("R          Cycle role"),
        Line::from("g          Set coordinator goal"),
        Line::from("p          Pause coordinator"),
        Line::from("r          Resume coordinator"),
        Line::from("x          Stop coordinator"),
        Line::from("q          Quit"),
        Line::from("?          Toggle this help"),
    ];

    let width = 44.min(area.width.saturating_sub(4));
    let height = (help_text.len() as u16 + 2).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup_area);

    let help = Paragraph::new(help_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Help")
            .style(Style::default().bg(Color::DarkGray)),
    );
    frame.render_widget(help, popup_area);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bundle::Bundle;
    use crate::domain::tick::Tick;
    use crate::domain::work::Work;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;

    fn test_terminal() -> Terminal<TestBackend> {
        let backend = TestBackend::new(80, 24);
        Terminal::new(backend).unwrap()
    }

    #[test]
    fn test_draw_default_app() {
        let app = App::new();
        let mut terminal = test_terminal();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    #[test]
    fn test_draw_with_help_overlay() {
        let mut app = App::new();
        app.show_help = true;
        let mut terminal = test_terminal();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    #[test]
    fn test_draw_each_view() {
        let mut app = App::new();
        for view in View::ALL {
            app.current_view = view;
            let mut terminal = test_terminal();
            terminal.draw(|frame| draw(&app, frame)).unwrap();
        }
    }

    #[test]
    fn test_draw_with_data() {
        let mut app = App::new();
        app.state
            .works
            .push(Work::new("ph1".into(), "Task 1".into(), "desc".into()));
        app.state.bundles.push(Bundle::new(
            "wi1".into(),
            None,
            "feature/test".into(),
            vec!["Test bundle".into()],
        ));
        app.state.ticks.push(Tick::new(1));

        // Render each view with data
        for view in View::ALL {
            app.current_view = view;
            let mut terminal = test_terminal();
            terminal.draw(|frame| draw(&app, frame)).unwrap();
        }
    }

    #[test]
    fn test_draw_each_role() {
        let mut app = App::new();
        let roles = [Role::Coordinator, Role::Integrator, Role::Implementer];
        for role in roles {
            app.current_role = role;
            let mut terminal = test_terminal();
            terminal.draw(|frame| draw(&app, frame)).unwrap();
        }
    }

    #[test]
    fn test_draw_connection_statuses() {
        let mut app = App::new();
        let statuses = [ConnectionStatus::Connected, ConnectionStatus::Disconnected];
        for status in statuses {
            app.connection = status;
            let mut terminal = test_terminal();
            terminal.draw(|frame| draw(&app, frame)).unwrap();
        }
    }

    #[test]
    fn test_role_actions_coordinator() {
        let actions = role_actions(Role::Coordinator);
        assert_eq!(actions.len(), 4);
        assert!(actions[0].contains("Pause"));
        assert!(actions[1].contains("Resume"));
        assert!(actions[2].contains("Stop"));
        assert!(actions[3].contains("Role"));
    }

    #[test]
    fn test_role_actions_integrator() {
        let actions = role_actions(Role::Integrator);
        assert_eq!(actions.len(), 3);
        assert!(actions[0].contains("Tick"));
    }

    #[test]
    fn test_role_actions_implementer() {
        let actions = role_actions(Role::Implementer);
        assert_eq!(actions.len(), 3);
        assert!(actions[0].contains("Work"));
    }

    #[test]
    fn test_draw_small_terminal() {
        let app = App::new();
        let backend = TestBackend::new(20, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    #[test]
    fn test_draw_help_small_terminal() {
        let mut app = App::new();
        app.show_help = true;
        let backend = TestBackend::new(30, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    /// Build a mock handshake response that passes version checking.
    fn mock_handshake(req: &crate::ipc::protocol::DaemonRequest) -> crate::ipc::protocol::DaemonResponse {
        let client_version = req
            .params
            .get("client_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        crate::ipc::protocol::DaemonResponse::ok(
            req.id,
            serde_json::json!({
                "protocol": "ndjson/1",
                "server_version": client_version,
                "client_version": client_version,
                "version_match": true,
                "session_id": "test-session",
            }),
        )
    }

    #[test]
    fn test_reconnect_interval_is_reasonable() {
        // Reconnect interval should be between 1 and 10 seconds
        assert!(RECONNECT_INTERVAL >= Duration::from_secs(1));
        assert!(RECONNECT_INTERVAL <= Duration::from_secs(10));
    }

    #[tokio::test]
    async fn test_try_connect_nonexistent_socket() {
        // try_connect should return None for a nonexistent socket
        let result = try_connect(Path::new("/tmp/nonexistent-loopr-reconnect-test.sock")).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_try_connect_succeeds_with_daemon() {
        use crate::ipc::protocol::DaemonEvent;
        use crate::ipc::server::{IpcServer, handle_client};

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("reconnect-{}.sock", crate::id::generate_id("xx")));

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    |req| mock_handshake(&req),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = try_connect(&path).await;
        assert!(result.is_some());
        let (_, session_id) = result.unwrap();
        assert_eq!(session_id, "test-session");

        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_reconnect_after_disconnect() {
        // Verify the app transitions from Disconnected back to Connected
        // when a new daemon becomes available
        use crate::ipc::protocol::DaemonEvent;
        use crate::ipc::server::{IpcServer, handle_client};
        

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("reconnect2-{}.sock", crate::id::generate_id("xx")));

        // Start disconnected, then start a daemon
        let mut app = App::new();
        app.connection = ConnectionStatus::Disconnected;
        assert_eq!(app.connection, ConnectionStatus::Disconnected);

        // Start a daemon
        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    |req| mock_handshake(&req),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Simulate reconnect logic: try_connect succeeds → update app state
        if let Some((_, session_id)) = try_connect(&path).await {
            app.connection = ConnectionStatus::Connected;
            app.session_id = session_id;
        }
        assert_eq!(app.connection, ConnectionStatus::Connected);
        assert_eq!(app.session_id, "test-session");

        let _ = server_handle.await;
    }

    #[test]
    fn test_event_collection_record_created() {
        use crate::ipc::protocol::DaemonEvent;
        let event = DaemonEvent::record_created("plan", "p1");
        assert_eq!(event_collection(&event), Some("plan"));
    }

    #[test]
    fn test_event_collection_record_updated() {
        use crate::ipc::protocol::DaemonEvent;
        let event = DaemonEvent::record_updated("learning", "l1");
        assert_eq!(event_collection(&event), Some("learning"));
    }

    #[test]
    fn test_event_collection_transition_completed() {
        use crate::ipc::protocol::DaemonEvent;
        let event = DaemonEvent::transition_completed("work", "wi1", "Draft", "Ready", "Coordinator");
        assert_eq!(event_collection(&event), Some("work"));
    }

    #[test]
    fn test_event_collection_tick_published() {
        use crate::ipc::protocol::DaemonEvent;
        let event = DaemonEvent::tick_published("t1", "abc123");
        assert_eq!(event_collection(&event), Some("tick"));
    }

    #[test]
    fn test_event_collection_bundle_rejected_stale() {
        use crate::ipc::protocol::DaemonEvent;
        let event = DaemonEvent::bundle_rejected_stale("wi1", "t1", "t2");
        assert_eq!(event_collection(&event), Some("bundle"));
    }

    #[test]
    fn test_event_collection_unknown_event() {
        use crate::ipc::protocol::DaemonEvent;
        let event = DaemonEvent::new("some.unknown.event", serde_json::json!({}));
        assert_eq!(event_collection(&event), None);
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_plans() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-{}.sock", crate::id::generate_id("xx")));

        // Create a mock plan to return
        let mock_plan = Plan::new("Test Plan".into(), "A test plan".into(), "Criteria".into());
        let plans_json = serde_json::to_value(vec![mock_plan.clone()]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let plans = plans_json.clone();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else if req.method == "plan.list" {
                            DaemonResponse::ok(req.id, plans.clone())
                        } else {
                            DaemonResponse::ok(req.id, json!(null))
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        assert!(state.plans.is_empty());

        refresh_collection(&mut state, &mut client, "plan").await;
        assert_eq!(state.plans.len(), 1);
        assert_eq!(state.plans[0].title, "Test Plan");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_works() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-wi-{}.sock", crate::id::generate_id("xx")));

        let mock_wi = Work::new("ph1".into(), "Task 1".into(), "desc".into());
        let wis_json = serde_json::to_value(vec![mock_wi.clone()]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let wis = wis_json.clone();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else if req.method == "work.list" {
                            DaemonResponse::ok(req.id, wis.clone())
                        } else {
                            DaemonResponse::ok(req.id, json!(null))
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        assert!(state.works.is_empty());

        refresh_collection(&mut state, &mut client, "work").await;
        assert_eq!(state.works.len(), 1);
        assert_eq!(state.works[0].title, "Task 1");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_unknown_collection_is_noop() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-unk-{}.sock", crate::id::generate_id("xx")));

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, json!([]))
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        // Refreshing an unknown collection should not panic or modify state
        refresh_collection(&mut state, &mut client, "unknown_collection").await;
        assert!(state.plans.is_empty());
        assert!(state.works.is_empty());

        drop(client);
        let _ = server_handle.await;
    }

    // --- Phase 4: Additional coverage tests ---

    #[test]
    fn test_draw_zero_size_terminal() {
        // Zero-size terminal should not panic
        let app = App::new();
        let backend = TestBackend::new(0, 0);
        let mut terminal = Terminal::new(backend).unwrap();
        // draw may not render anything but should not panic
        let _ = terminal.draw(|frame| draw(&app, frame));
    }

    #[test]
    fn test_draw_goal_input_mode() {
        let mut app = App::new();
        app.input_mode = InputMode::GoalInput;
        app.goal_input = "Build auth".to_string();
        let mut terminal = test_terminal();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    #[test]
    fn test_draw_goal_input_empty() {
        let mut app = App::new();
        app.input_mode = InputMode::GoalInput;
        app.goal_input.clear();
        let mut terminal = test_terminal();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    #[test]
    fn test_draw_goal_input_small_terminal() {
        let mut app = App::new();
        app.input_mode = InputMode::GoalInput;
        app.goal_input = "Goal".to_string();
        let backend = TestBackend::new(10, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    #[test]
    fn test_role_actions_reviewer() {
        let actions = role_actions(Role::Reviewer);
        assert_eq!(actions.len(), 2);
        assert!(actions[0].contains("Transition"));
    }

    #[test]
    fn test_role_actions_researcher() {
        let actions = role_actions(Role::Researcher);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].contains("Role"));
    }

    #[test]
    fn test_event_collection_agent_status_changed() {
        use crate::ipc::protocol::DaemonEvent;
        let event = DaemonEvent::agent_status_changed("s1", crate::agents::AgentStatus::Running);
        assert_eq!(event_collection(&event), Some("agent"));
    }

    #[test]
    fn test_event_collection_agent_llm_output() {
        use crate::ipc::protocol::DaemonEvent;
        let event = DaemonEvent::new(
            "agent.llm_output",
            serde_json::json!({"session_id": "s1", "chunk": "hello", "is_final": false}),
        );
        assert_eq!(event_collection(&event), Some("agent"));
    }

    #[test]
    fn test_event_collection_tick_validation_failed() {
        use crate::ipc::protocol::DaemonEvent;
        let event = DaemonEvent::new(
            "tick.validation_failed",
            serde_json::json!({"tick_id": "t1", "reason": "test failed"}),
        );
        assert_eq!(event_collection(&event), Some("tick"));
    }

    #[test]
    fn test_draw_with_agent_sessions() {
        use crate::agents::{AgentSession, AgentType};
        let mut app = App::new();
        app.current_view = View::Agents;
        app.state
            .agent_sessions
            .push(AgentSession::new(AgentType::Implementer, "test-model".to_string()));
        let mut terminal = test_terminal();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    #[test]
    fn test_draw_with_learnings() {
        use crate::domain::learning::{Learning, LearningScope};
        let mut app = App::new();
        app.current_view = View::Learnings;
        app.state.learnings.push(Learning::new(
            "wi-1".into(),
            LearningScope::Global,
            "Test insight".into(),
        ));
        let mut terminal = test_terminal();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    #[test]
    fn test_draw_with_locks() {
        use crate::domain::lock::Lock;
        let mut app = App::new();
        app.current_view = View::Locks;
        app.state
            .locks
            .push(Lock::new("src/main.rs".into(), "wi-1".into(), "coordinator".into()));
        let mut terminal = test_terminal();
        terminal.draw(|frame| draw(&app, frame)).unwrap();
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_bundles() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-bundle-{}.sock", crate::id::generate_id("xx")));

        let mock_bundle = Bundle::new("wi-1".into(), None, "feature/test".into(), vec!["Test bundle".into()]);
        let bundles_json = serde_json::to_value(vec![mock_bundle]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let bundles = bundles_json.clone();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else if req.method == "bundle.list" {
                            DaemonResponse::ok(req.id, bundles.clone())
                        } else {
                            DaemonResponse::ok(req.id, json!(null))
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        assert!(state.bundles.is_empty());
        refresh_collection(&mut state, &mut client, "bundle").await;
        assert_eq!(state.bundles.len(), 1);

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_ticks() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-tick-{}.sock", crate::id::generate_id("xx")));

        let mock_tick = Tick::new(1);
        let ticks_json = serde_json::to_value(vec![mock_tick]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let ticks = ticks_json.clone();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else if req.method == "tick.list" {
                            DaemonResponse::ok(req.id, ticks.clone())
                        } else {
                            DaemonResponse::ok(req.id, json!(null))
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        assert!(state.ticks.is_empty());
        refresh_collection(&mut state, &mut client, "tick").await;
        assert_eq!(state.ticks.len(), 1);

        drop(client);
        let _ = server_handle.await;
    }

    /// Helper: start a mock daemon server that records the method+params of each request.
    /// Returns the socket path, server handle, and a channel to read captured requests.
    async fn mock_ipc_server() -> (
        std::path::PathBuf,
        tokio::task::JoinHandle<()>,
        Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    ) {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;
        use std::sync::Mutex;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("dispatch-{}.sock", crate::id::generate_id("xx")));

        let captured: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    move |req| {
                        if req.method != "system.handshake" {
                            captured_clone
                                .lock()
                                .unwrap()
                                .push((req.method.clone(), req.params.clone()));
                        }
                        DaemonResponse::ok(req.id, json!({"ok": true}))
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        (path, handle, captured)
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_set_goal() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(&mut client, IpcAction::SetGoal("Build auth system".to_string())).await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "coordinator.set_goal");
            assert_eq!(reqs[0].1["goal"], "Build auth system");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_pause_agent() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(&mut client, IpcAction::PauseAgent("sess-1".to_string())).await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "agent.pause");
            assert_eq!(reqs[0].1["session_id"], "sess-1");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_resume_agent() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(&mut client, IpcAction::ResumeAgent("sess-2".to_string())).await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "agent.resume");
            assert_eq!(reqs[0].1["session_id"], "sess-2");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_stop_agent() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(&mut client, IpcAction::StopAgent("sess-3".to_string())).await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "agent.stop");
            assert_eq!(reqs[0].1["session_id"], "sess-3");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_new_record() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(
            &mut client,
            IpcAction::NewRecord {
                collection: "work".to_string(),
            },
        )
        .await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "work.create");
            assert_eq!(reqs[0].1["title"], "New Record");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_dispatch_ipc_action_transition_record() {
        let (path, server_handle, captured) = mock_ipc_server().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        dispatch_ipc_action(
            &mut client,
            IpcAction::TransitionRecord {
                collection: "bundle".to_string(),
                id: "b-123".to_string(),
            },
        )
        .await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "bundle.transition");
            assert_eq!(reqs[0].1["id"], "b-123");
            assert_eq!(reqs[0].1["target_status"], "Active");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_json_parse_error() {
        // Test that refresh_collection handles JSON that is valid but doesn't
        // match the expected type — the state should remain unchanged.
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-parse-err-{}.sock", crate::id::generate_id("xx")));

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            // Return a string instead of an array — will fail deserialization
                            DaemonResponse::ok(req.id, json!("not an array"))
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();

        // Each collection should fail silently and leave state empty
        for collection in [
            "plan", "spec", "phase", "work", "bundle", "tick", "learning", "lock", "agent",
        ] {
            refresh_collection(&mut state, &mut client, collection).await;
        }
        assert!(state.plans.is_empty());
        assert!(state.specs.is_empty());
        assert!(state.phases.is_empty());
        assert!(state.works.is_empty());
        assert!(state.bundles.is_empty());
        assert!(state.ticks.is_empty());
        assert!(state.learnings.is_empty());
        assert!(state.locks.is_empty());
        assert!(state.agent_sessions.is_empty());

        drop(client);
        let _ = server_handle.await;
    }

    #[test]
    fn test_render_header_highlighting() {
        // Verify the header renders correctly with different selected views
        let mut app = App::new();
        let mut terminal = test_terminal();

        for view in View::ALL {
            app.current_view = view;
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_header(&app, frame, area);
                })
                .unwrap();
        }
    }

    #[test]
    fn test_render_content_all_views() {
        // Verify render_content renders without panic for every view, including with data
        let mut app = App::new();
        app.state
            .works
            .push(Work::new("ph1".into(), "WI 1".into(), "desc".into()));
        app.state.bundles.push(Bundle::new(
            "wi1".into(),
            None,
            "feature/test".into(),
            vec!["claims".into()],
        ));
        app.state.ticks.push(Tick::new(1));
        app.state.learnings.push(crate::domain::learning::Learning::new(
            "wi-1".into(),
            crate::domain::learning::LearningScope::Global,
            "insight".into(),
        ));
        app.state.locks.push(crate::domain::lock::Lock::new(
            "src/main.rs".into(),
            "wi-1".into(),
            "coord".into(),
        ));
        app.state.agent_sessions.push(crate::agents::AgentSession::new(
            crate::agents::AgentType::Coordinator,
            "model".to_string(),
        ));

        let mut terminal = test_terminal();
        for view in View::ALL {
            app.current_view = view;
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    render_content(&app, frame, area);
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_specs() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-spec-{}.sock", crate::id::generate_id("xx")));

        let mock_spec = Spec::new("plan-1".into(), "Test Spec".into(), "Desc".into());
        let specs_json = serde_json::to_value(vec![mock_spec]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let specs = specs_json.clone();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, specs.clone())
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "spec").await;
        assert_eq!(state.specs.len(), 1);
        assert_eq!(state.specs[0].title, "Test Spec");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_phases() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-phase-{}.sock", crate::id::generate_id("xx")));

        let mock_phase = Phase::new("spec-1".into(), "Phase 1".into(), "Desc".into(), 1);
        let phases_json = serde_json::to_value(vec![mock_phase]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let phases = phases_json.clone();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, phases.clone())
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "phase").await;
        assert_eq!(state.phases.len(), 1);
        assert_eq!(state.phases[0].title, "Phase 1");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_learnings() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-learning-{}.sock", crate::id::generate_id("xx")));

        let mock_learning = Learning::new(
            "wi-1".into(),
            crate::domain::learning::LearningScope::Global,
            "insight".into(),
        );
        let learnings_json = serde_json::to_value(vec![mock_learning]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let learnings = learnings_json.clone();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, learnings.clone())
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "learning").await;
        assert_eq!(state.learnings.len(), 1);
        assert_eq!(state.learnings[0].content, "insight");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_locks() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-lock-{}.sock", crate::id::generate_id("xx")));

        let mock_lock = Lock::new("src/main.rs".into(), "wi-1".into(), "coordinator".into());
        let locks_json = serde_json::to_value(vec![mock_lock]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let locks = locks_json.clone();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, locks.clone())
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "lock").await;
        assert_eq!(state.locks.len(), 1);
        assert_eq!(state.locks[0].resource, "src/main.rs");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_agent_sessions() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-agent-{}.sock", crate::id::generate_id("xx")));

        let mock_session = AgentSession::new(crate::agents::AgentType::Implementer, "test-model".to_string());
        let sessions_json = serde_json::to_value(vec![mock_session]).unwrap();

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                let sessions = sessions_json.clone();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            DaemonResponse::ok(req.id, sessions.clone())
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        refresh_collection(&mut state, &mut client, "agent").await;
        assert_eq!(state.agent_sessions.len(), 1);

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_refresh_collection_invalid_json_no_panic() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-badjson-{}.sock", crate::id::generate_id("xx")));

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    move |req| {
                        if req.method == "system.handshake" {
                            mock_handshake(&req)
                        } else {
                            // Return a non-array value that won't deserialize as Vec<T>
                            DaemonResponse::ok(req.id, json!({"not": "an array"}))
                        }
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        client.handshake("0.1.0").await.unwrap();

        let mut state = AppState::default();
        // Should not panic — deserialization failure is silently ignored
        refresh_collection(&mut state, &mut client, "plan").await;
        assert!(state.plans.is_empty());
        refresh_collection(&mut state, &mut client, "spec").await;
        assert!(state.specs.is_empty());
        refresh_collection(&mut state, &mut client, "phase").await;
        assert!(state.phases.is_empty());
        refresh_collection(&mut state, &mut client, "work").await;
        assert!(state.works.is_empty());
        refresh_collection(&mut state, &mut client, "bundle").await;
        assert!(state.bundles.is_empty());
        refresh_collection(&mut state, &mut client, "tick").await;
        assert!(state.ticks.is_empty());
        refresh_collection(&mut state, &mut client, "learning").await;
        assert!(state.learnings.is_empty());
        refresh_collection(&mut state, &mut client, "lock").await;
        assert!(state.locks.is_empty());
        refresh_collection(&mut state, &mut client, "agent").await;
        assert!(state.agent_sessions.is_empty());

        drop(client);
        let _ = server_handle.await;
    }

    #[test]
    fn test_system_prompt_chat_state() {
        let prompt = chat::system_prompt_for_chat(FunnelState::Chat, false);
        assert!(prompt.contains("Loopr development orchestrator"));
        assert!(!prompt.contains("clarifying questions"));
    }

    #[test]
    fn test_system_prompt_interview_state() {
        let prompt = chat::system_prompt_for_chat(FunnelState::Interview, false);
        assert!(prompt.contains("Loopr development orchestrator"));
        assert!(prompt.contains("clarifying questions"));
    }

    #[test]
    fn test_system_prompt_draft_request() {
        let prompt = chat::system_prompt_for_chat(FunnelState::PlanDraft, true);
        assert!(prompt.contains("structured plan"));
        assert!(!prompt.contains("refine"));
    }

    #[test]
    fn test_system_prompt_plan_refine() {
        let prompt = chat::system_prompt_for_chat(FunnelState::PlanDraft, false);
        assert!(prompt.contains("refine"));
        assert!(!prompt.contains("structured plan"));
    }

    #[test]
    fn test_system_prompt_executing_state() {
        let prompt = chat::system_prompt_for_chat(FunnelState::Executing, false);
        assert!(prompt.contains("Loopr development orchestrator"));
        assert!(!prompt.contains("clarifying questions"));
    }

    #[test]
    fn test_canonical_messages_lifecycle() {
        let mut app = App::new();
        assert!(app.canonical_messages.is_empty());

        // Simulate user message
        app.canonical_messages.push(crate::tools::types::Message {
            role: "user".to_string(),
            content: vec![crate::tools::types::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        });
        assert_eq!(app.canonical_messages.len(), 1);
        assert_eq!(app.canonical_messages[0].role, "user");

        // Simulate assistant response with tool use
        app.canonical_messages.push(crate::tools::types::Message {
            role: "assistant".to_string(),
            content: vec![crate::tools::types::ContentBlock::Text {
                text: "hi there".to_string(),
            }],
        });
        assert_eq!(app.canonical_messages.len(), 2);

        // Clear resets both
        app.chat_history.clear();
        app.canonical_messages.clear();
        assert!(app.canonical_messages.is_empty());
    }

    #[test]
    fn test_draft_request_appends_synthetic_user_message() {
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

        // Before fix: messages ends with assistant — API would reject
        assert_eq!(canonical.last().unwrap().role, "assistant");

        // Apply the same logic as the event loop
        let is_draft_request = true;
        if canonical.last().map(|m| m.role.as_str()) != Some("user") {
            let synthetic = if is_draft_request { chat::DRAFT_PROMPT } else { "fallback" };
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
}
