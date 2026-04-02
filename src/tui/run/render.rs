use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::domain::role::Role;
use crate::tui::app::{App, ChatMode, ConnectionStatus, FunnelState, InputMode, View, colors};
use crate::tui::views;

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
pub fn render_header(app: &App, frame: &mut Frame, area: Rect) {
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
pub fn render_content(app: &App, frame: &mut Frame, area: Rect) {
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
    use ratatui::Terminal;
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
    fn test_draw_with_agent_sessions() {
        use crate::agents::{AgentKind, AgentSession};
        let mut app = App::new();
        app.current_view = View::Agents;
        app.state
            .agent_sessions
            .push(AgentSession::new(AgentKind::Implementer, "test-model".to_string()));
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
            crate::agents::AgentKind::Coordinator,
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
}
