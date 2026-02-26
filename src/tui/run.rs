use std::io;
use std::path::Path;

use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs};
use ratatui::{Frame, Terminal};

use crate::domain::role::Role;
use crate::ipc::client::IpcClient;
use crate::ipc::protocol::IpcMessage;

use super::app::{App, ConnectionStatus, View};
use super::input::{apply_action, handle_key};
use super::views;

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
    let result = event_loop(&mut terminal, &mut app, &mut client).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    result
}

/// Main select! loop: keyboard events + IPC messages.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    client: &mut IpcClient,
) -> eyre::Result<()> {
    let mut events = EventStream::new();

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
                    let action = handle_key(key);
                    apply_action(app, action);
                }
            }
            ipc_msg = client.recv() => {
                match ipc_msg {
                    Ok(Some(IpcMessage::Event(_event))) => {
                        // TODO: update app.state based on daemon events
                    }
                    Ok(None) | Err(_) => {
                        app.connection = ConnectionStatus::Disconnected;
                    }
                    Ok(Some(IpcMessage::Response(_))) => {
                        // Unsolicited response — ignore
                    }
                }
            }
        }
    }

    Ok(())
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
        Role::Coordinator => vec!["n:New Plan", "t:Transition", "r:Switch Role"],
        Role::Integrator => vec!["n:New Tick", "t:Transition", "r:Switch Role"],
        Role::Implementer => vec!["n:New WorkItem", "t:Transition", "r:Switch Role"],
    }
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
        Line::from("r          Cycle role"),
        Line::from("n          New item (context-dependent)"),
        Line::from("t          Transition selected item"),
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
        let statuses = [
            ConnectionStatus::Connected,
            ConnectionStatus::Disconnected,
        ];
        for status in statuses {
            app.connection = status;
            let mut terminal = test_terminal();
            terminal.draw(|frame| draw(&app, frame)).unwrap();
        }
    }

    #[test]
    fn test_role_actions_coordinator() {
        let actions = role_actions(Role::Coordinator);
        assert_eq!(actions.len(), 3);
        assert!(actions[0].contains("Plan"));
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
}
