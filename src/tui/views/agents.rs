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
            let target = match (&session.work_item_id, &session.bundle_id) {
                (Some(wi), _) => format!(" wi:{}", &wi[..wi.len().min(8)]),
                (_, Some(b)) => format!(" b:{}", &b[..b.len().min(8)]),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentSession, AgentType};

    #[test]
    fn test_render_empty_does_not_panic() {
        let app = App::new();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn test_render_with_sessions_does_not_panic() {
        let mut app = App::new();
        let mut s1 = AgentSession::new(AgentType::Implementer, "claude-sonnet-4-6".to_string());
        s1.work_item_id = Some("wi-abc123".to_string());
        s1.iteration = 3;
        let _ = s1.transition_to(AgentStatus::Running);
        app.state.agent_sessions.push(s1);

        let mut s2 = AgentSession::new(AgentType::Reviewer, "claude-sonnet-4-6".to_string());
        s2.bundle_id = Some("b-def456".to_string());
        let _ = s2.transition_to(AgentStatus::Running);
        let _ = s2.transition_to(AgentStatus::Completed);
        app.state.agent_sessions.push(s2);

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
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
            let mut session = AgentSession::new(AgentType::Implementer, "m".to_string());
            // Transition through valid path to reach desired status
            match terminal_status {
                AgentStatus::Starting => {} // already there
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

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }
}
