use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::app::App;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Role + connection status
            Constraint::Min(5),    // Queue counts
        ])
        .split(area);

    // Status header
    let status_line = Line::from(vec![
        Span::styled("Role: ", Style::default().fg(Color::Gray)),
        Span::styled(
            app.current_role.to_string(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  |  "),
        Span::styled("Connection: ", Style::default().fg(Color::Gray)),
        Span::styled(
            app.connection.to_string(),
            Style::default().fg(match app.connection {
                crate::tui::app::ConnectionStatus::Connected => Color::Green,
                crate::tui::app::ConnectionStatus::Disconnected => Color::Red,
            }),
        ),
    ]);
    let status_block = Paragraph::new(status_line).block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(status_block, chunks[0]);

    // Queue counts
    let counts = vec![
        Line::from(format!(
            "Plans: {}  Specs: {}  Phases: {}",
            app.state.plans.len(),
            app.state.specs.len(),
            app.state.phases.len(),
        )),
        Line::from(format!(
            "Works: {}  Bundles: {}",
            app.state.works.len(),
            app.state.bundles.len(),
        )),
        Line::from(format!(
            "Ticks: {}  Learnings: {}  Locks: {}",
            app.state.ticks.len(),
            app.state.learnings.len(),
            app.state.locks.len(),
        )),
    ];
    let counts_block = Paragraph::new(counts).block(Block::default().borders(Borders::ALL).title("Overview"));
    frame.render_widget(counts_block, chunks[1]);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_does_not_panic() {
        let app = App::new();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }
}
