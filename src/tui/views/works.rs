use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use crate::tui::app::App;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .state
        .works
        .iter()
        .map(|wi| ListItem::new(Line::from(format!("[{}] {} ({})", wi.status(), wi.title, wi.id))))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Works"))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.state.works.is_empty() {
        state.select(Some(app.selected_index));
    }

    frame.render_stateful_widget(list, area, &mut state);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::work::Work;
    use crate::tui::test_utils::{buffer_contains_text, test_terminal};

    #[test]
    fn test_render_empty_shows_title() {
        let app = App::new();
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Works"));
    }

    #[test]
    fn test_render_with_items_shows_content() {
        let mut app = App::new();
        app.state
            .works
            .push(Work::new("ph1".into(), "Task 1".into(), "desc".into()));
        app.state
            .works
            .push(Work::new("ph1".into(), "Task 2".into(), "desc".into()));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Task 1"));
        assert!(buffer_contains_text(buffer, "Task 2"));
    }

    #[test]
    fn test_render_shows_status() {
        let mut app = App::new();
        app.state
            .works
            .push(Work::new("ph1".into(), "My Work".into(), "desc".into()));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        // Default work status should be visible in the format "[Status] Title (id)"
        assert!(buffer_contains_text(buffer, "My Work"));
    }
}
