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
        .map(|wi| ListItem::new(Line::from(format!("[{}] {} ({})", wi.status, wi.title, wi.id))))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Work Items"))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.state.works.is_empty() {
        state.select(Some(app.selected_index));
    }

    frame.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::work::Work;

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
    fn test_render_with_items_does_not_panic() {
        let mut app = App::new();
        app.state
            .works
            .push(Work::new("ph1".into(), "Task 1".into(), "desc".into()));
        app.state
            .works
            .push(Work::new("ph1".into(), "Task 2".into(), "desc".into()));

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }
}
