use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use crate::agents::AgentStatus;
use crate::tui::app::App;

/// Color for an agent status indicator.
fn status_color(status: AgentStatus) -> Color {
    match status {
        AgentStatus::Starting => Color::Yellow,
        AgentStatus::Running | AgentStatus::WaitingForLlm => Color::Green,
        AgentStatus::Paused => Color::Cyan,
        AgentStatus::Idle => Color::Gray,
        AgentStatus::Completed => Color::Blue,
        AgentStatus::Failed => Color::Red,
        AgentStatus::Cancelled => Color::DarkGray,
    }
}

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .state
        .agent_sessions
        .iter()
        .map(|session| {
            let target = match (&session.work_id, &session.bundle_id, &session.target_id, &session.query) {
                (Some(wi), _, _, _) => format!(" wi:{}", &wi[..wi.len().min(8)]),
                (_, Some(b), _, _) => format!(" b:{}", &b[..b.len().min(8)]),
                (_, _, Some(t), Some(q)) => {
                    let q_trunc = if q.len() > 20 { &q[..20] } else { q };
                    format!(" {}:{}", &t[..t.len().min(8)], q_trunc)
                }
                (_, _, Some(t), None) => format!(" target:{}", &t[..t.len().min(8)]),
                (_, _, None, Some(q)) => {
                    let q_trunc = if q.len() > 24 { &q[..24] } else { q };
                    format!(" q:{}", q_trunc)
                }
                _ => String::new(),
            };
            let iter_info = if session.iteration > 0 {
                format!(" iter:{}", session.iteration)
            } else {
                String::new()
            };
            let line = format!(
                "[{}] {} ({}){}{}",
                session.status,
                session.agent_type,
                &session.id[..session.id.len().min(8)],
                target,
                iter_info
            );
            ListItem::new(Line::from(line)).style(Style::default().fg(status_color(session.status)))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Agents"))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.state.agent_sessions.is_empty() {
        state.select(Some(app.selected_index));
    }

    frame.render_stateful_widget(list, area, &mut state);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentSession, AgentKind};
    use crate::tui::test_utils::{buffer_contains_text, test_terminal};

    #[test]
    fn test_render_empty_shows_title() {
        let app = App::new();
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Agents"));
    }

    #[test]
    fn test_render_with_sessions_shows_details() {
        let mut app = App::new();
        let mut s1 = AgentSession::new(AgentKind::Implementer, "claude-sonnet-4-6".to_string());
        s1.work_id = Some("wi-abc123".to_string());
        s1.iteration = 3;
        let _ = s1.transition_to(AgentStatus::Running);
        app.state.agent_sessions.push(s1);

        let mut s2 = AgentSession::new(AgentKind::Reviewer, "claude-sonnet-4-6".to_string());
        s2.bundle_id = Some("b-def456".to_string());
        let _ = s2.transition_to(AgentStatus::Running);
        let _ = s2.transition_to(AgentStatus::Completed);
        app.state.agent_sessions.push(s2);

        let mut s3 = AgentSession::new(AgentKind::Researcher, "claude-sonnet-4-6".to_string());
        s3.target_id = Some("wi-xyz789".to_string());
        s3.query = Some("Investigate auth module".to_string());
        let _ = s3.transition_to(AgentStatus::Running);
        app.state.agent_sessions.push(s3);

        let s4 = AgentSession::new(AgentKind::Coordinator, "claude-sonnet-4-6".to_string());
        app.state.agent_sessions.push(s4);

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "implementer"));
        assert!(buffer_contains_text(buffer, "reviewer"));
        assert!(buffer_contains_text(buffer, "researcher"));
        assert!(buffer_contains_text(buffer, "coordinator"));
        assert!(buffer_contains_text(buffer, "iter:3"));
        assert!(buffer_contains_text(buffer, "Investigate auth"));
    }

    #[test]
    fn test_status_color_mapping() {
        assert_eq!(status_color(AgentStatus::Starting), Color::Yellow);
        assert_eq!(status_color(AgentStatus::Running), Color::Green);
        assert_eq!(status_color(AgentStatus::WaitingForLlm), Color::Green);
        assert_eq!(status_color(AgentStatus::Paused), Color::Cyan);
        assert_eq!(status_color(AgentStatus::Completed), Color::Blue);
        assert_eq!(status_color(AgentStatus::Failed), Color::Red);
        assert_eq!(status_color(AgentStatus::Cancelled), Color::DarkGray);
    }

    #[test]
    fn test_render_all_statuses() {
        let mut app = App::new();
        let statuses = [
            AgentStatus::Starting,
            AgentStatus::Running,
            AgentStatus::Completed,
            AgentStatus::Failed,
            AgentStatus::Cancelled,
        ];
        for (i, &terminal_status) in statuses.iter().enumerate() {
            let mut session = AgentSession::new(AgentKind::Implementer, "m".to_string());
            match terminal_status {
                AgentStatus::Starting => {}
                AgentStatus::Running => {
                    let _ = session.transition_to(AgentStatus::Running);
                }
                AgentStatus::Completed => {
                    let _ = session.transition_to(AgentStatus::Running);
                    let _ = session.transition_to(AgentStatus::Completed);
                }
                AgentStatus::Failed => {
                    let _ = session.transition_to(AgentStatus::Running);
                    let _ = session.transition_to(AgentStatus::Failed);
                }
                AgentStatus::Cancelled => {
                    let _ = session.transition_to(AgentStatus::Cancelled);
                }
                _ => {}
            }
            session.iteration = i as u32;
            app.state.agent_sessions.push(session);
        }

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "starting"));
        assert!(buffer_contains_text(buffer, "running"));
        assert!(buffer_contains_text(buffer, "completed"));
        assert!(buffer_contains_text(buffer, "failed"));
        assert!(buffer_contains_text(buffer, "cancelled"));
    }
}
