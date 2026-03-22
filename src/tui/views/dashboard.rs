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
    use crate::tui::test_utils::{buffer_contains_text, test_terminal};

    #[test]
    fn test_render_shows_status_labels() {
        let app = App::new();
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Role:"));
        assert!(buffer_contains_text(buffer, "Connection:"));
        assert!(buffer_contains_text(buffer, "Status"));
        assert!(buffer_contains_text(buffer, "Overview"));
    }

    #[test]
    fn test_render_shows_default_role() {
        let app = App::new();
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "coordinator"));
    }

    #[test]
    fn test_render_shows_disconnected() {
        let app = App::new();
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Disconnected"));
    }

    #[test]
    fn test_render_shows_connected() {
        let mut app = App::new();
        app.connection = crate::tui::app::ConnectionStatus::Connected;
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Connected"));
    }

    #[test]
    fn test_render_shows_queue_counts() {
        let app = App::new();
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Plans: 0"));
        assert!(buffer_contains_text(buffer, "Specs: 0"));
        assert!(buffer_contains_text(buffer, "Works: 0"));
        assert!(buffer_contains_text(buffer, "Bundles: 0"));
        assert!(buffer_contains_text(buffer, "Ticks: 0"));
        assert!(buffer_contains_text(buffer, "Learnings: 0"));
        assert!(buffer_contains_text(buffer, "Locks: 0"));
    }

    #[test]
    fn test_render_shows_nonzero_counts() {
        let mut app = App::new();
        app.state
            .works
            .push(crate::domain::work::Work::new("ph1".into(), "t1".into(), "d1".into()));
        app.state
            .works
            .push(crate::domain::work::Work::new("ph1".into(), "t2".into(), "d2".into()));
        app.state.ticks.push(crate::domain::tick::Tick::new(1));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Works: 2"));
        assert!(buffer_contains_text(buffer, "Ticks: 1"));
    }
}
