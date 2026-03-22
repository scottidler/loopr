use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use crate::tui::app::App;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .state
        .learnings
        .iter()
        .map(|l| {
            ListItem::new(Line::from(format!(
                "[{}] {} (+{}/-{}) {}",
                l.scope,
                l.content.chars().take(50).collect::<String>(),
                l.reinforcements,
                l.contradictions,
                if l.promoted { "[promoted]" } else { "" },
            )))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Learnings"))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.state.learnings.is_empty() {
        state.select(Some(app.selected_index));
    }

    frame.render_stateful_widget(list, area, &mut state);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::learning::{Learning, LearningScope};
    use crate::tui::test_utils::{buffer_contains_text, test_terminal};

    #[test]
    fn test_render_empty_shows_title() {
        let app = App::new();
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Learnings"));
    }

    #[test]
    fn test_render_with_learnings_shows_content() {
        let mut app = App::new();
        app.state.learnings.push(Learning::new(
            "src1".into(),
            LearningScope::Work,
            "Test learning content".into(),
        ));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Test learning content"));
    }

    #[test]
    fn test_render_promoted_learning() {
        let mut app = App::new();
        let mut learning = Learning::new("src1".into(), LearningScope::Work, "Promoted insight".into());
        learning.promoted = true;

        app.state.learnings.push(learning);

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "[promoted]"));
        assert!(buffer_contains_text(buffer, "Promoted insight"));
    }
}
