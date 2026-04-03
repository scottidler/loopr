use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use crate::tui::app::App;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .state
        .bundles
        .iter()
        .map(|b| ListItem::new(Line::from(format!("[{}] {} ({})", b.status(), b.branch_name, b.id))))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Bundles"))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.state.bundles.is_empty() {
        state.select(Some(app.selected_index));
    }

    frame.render_stateful_widget(list, area, &mut state);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::bundle::Bundle;
    use crate::tui::test_utils::{buffer_contains_text, test_terminal};

    #[test]
    fn test_render_empty_shows_title() {
        let app = App::new();
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Bundles"));
    }

    #[test]
    fn test_render_with_bundles_shows_branch() {
        let mut app = App::new();
        app.state.bundles.push(Bundle::new(
            "wi-1".into(),
            None,
            "feat/my-feature".into(),
            vec!["implements widget".into()],
        ));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "feat/my-feature"));
    }
}
