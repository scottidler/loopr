use std::io;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use log::{info, warn};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs};
use ratatui::{Frame, Terminal};
use tokio::time::Interval;

use crate::agents::AgentSession;
use crate::domain::bundle::Bundle;
use crate::domain::learning::Learning;
use crate::domain::lock::Lock;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::role::Role;
use crate::domain::spec::Spec;
use crate::domain::tick::Tick;
use crate::domain::work_item::WorkItem;
use crate::ipc::client::IpcClient;
use crate::ipc::protocol::IpcMessage;

use super::app::{App, AppState, ConnectionStatus, InputMode, IpcAction, View};
use super::input::{apply_action, handle_key};
use super::views;

/// Reconnect interval when disconnected from daemon.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// Run the TUI, connecting to the daemon at the given socket path.
pub async fn run_tui(socket_path: &Path) -> eyre::Result<()> {
    // Connect to daemon
    let mut client = IpcClient::connect(socket_path)
        .await
        .map_err(|e| eyre::eyre!("Failed to connect to daemon: {e}"))?;
    client
        .handshake(env!("CARGO_PKG_VERSION"))
        .await
        .map_err(|e| eyre::eyre!("Handshake failed: {e}"))?;

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app state
    let mut app = App::new();
    app.connection = ConnectionStatus::Connected;

    // Run event loop; capture result so we always restore the terminal
    let result = event_loop(&mut terminal, &mut app, Some(client), socket_path).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

/// Try to connect and handshake with the daemon.
async fn try_connect(socket_path: &Path) -> Option<IpcClient> {
    let mut client = IpcClient::connect(socket_path).await.ok()?;
    client.handshake(env!("CARGO_PKG_VERSION")).await.ok()?;
    Some(client)
}

/// Main select! loop: keyboard events + IPC messages + reconnection.
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
        terminal.draw(|frame| draw(app, frame))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            crossterm_event = events.next() => {
                if let Some(Ok(Event::Key(key))) = crossterm_event
                    && key.kind == KeyEventKind::Press
                {
                    let action = handle_key(key, app.input_mode);
                    apply_action(app, action);

                    // Dispatch any pending IPC action
                    if let (Some(ipc_action), Some(c)) = (app.pending_ipc.take(), client.as_mut()) {
                        dispatch_ipc_action(c, ipc_action).await;
                    }
                }
            }
            ipc_msg = async { client.as_mut().unwrap().recv().await }, if client.is_some() => {
                match ipc_msg {
                    Ok(Some(IpcMessage::Event(event))) => {
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
                    Ok(Some(IpcMessage::Response(_))) => {
                        // Unsolicited response — ignore
                    }
                }
            }
            _ = reconnect_timer.tick(), if client.is_none() => {
                if let Some(new_client) = try_connect(socket_path).await {
                    info!("Reconnected to daemon");
                    app.connection = ConnectionStatus::Connected;
                    client = Some(new_client);
                }
            }
        }
    }

    Ok(())
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
                    "work_item" => {
                        if let Ok(items) = serde_json::from_value::<Vec<WorkItem>>(result) {
                            state.work_items = items;
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

/// Draw the full TUI frame: tab bar, content area, action bar, and optional help overlay.
pub fn draw(app: &App, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Tab bar
            Constraint::Min(5),    // Main content
            Constraint::Length(3), // Action bar
        ])
        .split(frame.area());

    draw_tab_bar(app, frame, chunks[0]);
    draw_content(app, frame, chunks[1]);
    draw_action_bar(app, frame, chunks[2]);

    if app.input_mode == InputMode::GoalInput {
        draw_goal_input(app, frame, frame.area());
    }

    if app.show_help {
        draw_help_overlay(frame, frame.area());
    }
}

/// Top tab bar showing all views with the current one highlighted.
fn draw_tab_bar(app: &App, frame: &mut Frame, area: Rect) {
    let titles: Vec<String> = View::ALL.iter().map(|v| v.to_string()).collect();

    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Loopr [{}]", app.current_role)),
        )
        .select(View::ALL.iter().position(|v| *v == app.current_view).unwrap_or(0))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));

    frame.render_widget(tabs, area);
}

/// Delegate to the current view's render function.
fn draw_content(app: &App, frame: &mut Frame, area: Rect) {
    match app.current_view {
        View::Dashboard => views::dashboard::render(app, frame, area),
        View::WorkItems => views::work_items::render(app, frame, area),
        View::Bundles => views::bundles::render(app, frame, area),
        View::Ticks => views::ticks::render(app, frame, area),
        View::Learnings => views::learnings::render(app, frame, area),
        View::Locks => views::locks::render(app, frame, area),
        View::Agents => views::agents::render(app, frame, area),
    }
}

/// Bottom bar showing role-specific actions and connection status.
fn draw_action_bar(app: &App, frame: &mut Frame, area: Rect) {
    let actions = role_actions(app.current_role);
    let action_line = Line::from(vec![
        Span::styled(
            format!("[{}] ", app.current_role),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(actions.join(" | "), Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled(
            app.connection.to_string(),
            Style::default().fg(match app.connection {
                ConnectionStatus::Connected => Color::Green,
                ConnectionStatus::Disconnected => Color::Red,
            }),
        ),
    ]);

    let bar = Paragraph::new(action_line).block(Block::default().borders(Borders::ALL).title("Actions [? help]"));
    frame.render_widget(bar, area);
}

/// Actions available for each role, shown in the action bar.
pub fn role_actions(role: Role) -> Vec<&'static str> {
    match role {
        Role::Coordinator => vec!["g:Goal", "p:Pause", "r:Resume", "x:Stop", "R:Role"],
        Role::Integrator => vec!["n:New Tick", "t:Transition", "R:Role"],
        Role::Implementer => vec!["n:New WorkItem", "t:Transition", "R:Role"],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bundle::Bundle;
    use crate::domain::tick::Tick;
    use crate::domain::work_item::WorkItem;
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
            .work_items
            .push(WorkItem::new("ph1".into(), "Task 1".into(), "desc".into()));
        app.state.bundles.push(Bundle::new(
            "wi1".into(),
            None,
            "feature/test".into(),
            "Test bundle".into(),
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
        assert_eq!(actions.len(), 5);
        assert!(actions[0].contains("Goal"));
        assert!(actions[1].contains("Pause"));
        assert!(actions[2].contains("Resume"));
        assert!(actions[3].contains("Stop"));
        assert!(actions[4].contains("Role"));
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
        assert!(actions[0].contains("WorkItem"));
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
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("reconnect-{}.sock", crate::id::generate_id()));

        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    |req| DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"})),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = try_connect(&path).await;
        assert!(result.is_some());

        drop(result);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_reconnect_after_disconnect() {
        // Verify the app transitions from Disconnected back to Connected
        // when a new daemon becomes available
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("reconnect2-{}.sock", crate::id::generate_id()));

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
                    |req| DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"})),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Simulate reconnect logic: try_connect succeeds → update app state
        if let Some(_client) = try_connect(&path).await {
            app.connection = ConnectionStatus::Connected;
        }
        assert_eq!(app.connection, ConnectionStatus::Connected);

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
        let event = DaemonEvent::transition_completed("work_item", "wi1", "Draft", "Ready", "Coordinator");
        assert_eq!(event_collection(&event), Some("work_item"));
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
        let path = dir.join(format!("refresh-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
    async fn test_refresh_collection_updates_work_items() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-wi-{}.sock", crate::id::generate_id()));

        let mock_wi = WorkItem::new("ph1".into(), "Task 1".into(), "desc".into());
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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
                        } else if req.method == "work_item.list" {
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
        assert!(state.work_items.is_empty());

        refresh_collection(&mut state, &mut client, "work_item").await;
        assert_eq!(state.work_items.len(), 1);
        assert_eq!(state.work_items[0].title, "Task 1");

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
        let path = dir.join(format!("refresh-unk-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
        assert!(state.work_items.is_empty());

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
        let path = dir.join(format!("refresh-bundle-{}.sock", crate::id::generate_id()));

        let mock_bundle = Bundle::new("wi-1".into(), None, "feature/test".into(), "Test bundle".into());
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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
        let path = dir.join(format!("refresh-tick-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
        let path = dir.join(format!("dispatch-{}.sock", crate::id::generate_id()));

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
                collection: "work_item".to_string(),
            },
        )
        .await;

        {
            let reqs = captured.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "work_item.create");
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
        let path = dir.join(format!("refresh-parse-err-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
            "plan",
            "spec",
            "phase",
            "work_item",
            "bundle",
            "tick",
            "learning",
            "lock",
            "agent",
        ] {
            refresh_collection(&mut state, &mut client, collection).await;
        }
        assert!(state.plans.is_empty());
        assert!(state.specs.is_empty());
        assert!(state.phases.is_empty());
        assert!(state.work_items.is_empty());
        assert!(state.bundles.is_empty());
        assert!(state.ticks.is_empty());
        assert!(state.learnings.is_empty());
        assert!(state.locks.is_empty());
        assert!(state.agent_sessions.is_empty());

        drop(client);
        let _ = server_handle.await;
    }

    #[test]
    fn test_draw_tab_bar_highlighting() {
        // Verify the tab bar renders correctly with different selected views
        let mut app = App::new();
        let mut terminal = test_terminal();

        for view in View::ALL {
            app.current_view = view;
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    draw_tab_bar(&app, frame, area);
                })
                .unwrap();
        }
    }

    #[test]
    fn test_draw_content_all_tabs() {
        // Verify draw_content renders without panic for every view, including with data
        let mut app = App::new();
        app.state
            .work_items
            .push(WorkItem::new("ph1".into(), "WI 1".into(), "desc".into()));
        app.state
            .bundles
            .push(Bundle::new("wi1".into(), None, "feature/test".into(), "claims".into()));
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
                    draw_content(&app, frame, area);
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_refresh_collection_updates_specs() {
        use crate::ipc::protocol::{DaemonEvent, DaemonResponse};
        use crate::ipc::server::{IpcServer, handle_client};
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-spec-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-phase-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-learning-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-lock-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
        use serde_json::json;

        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("refresh-agent-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
        let path = dir.join(format!("refresh-badjson-{}.sock", crate::id::generate_id()));

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
                            DaemonResponse::ok(req.id, json!({"protocol": "ndjson/1"}))
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
        refresh_collection(&mut state, &mut client, "work_item").await;
        assert!(state.work_items.is_empty());
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
}
