use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use crate::tui::app::App;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .state
        .ticks
        .iter()
        .map(|t| {
            let sha = t.integration_sha.as_deref().unwrap_or("none");
            ListItem::new(Line::from(format!("Tick #{} [{}] SHA: {}", t.number, t.status, sha)))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Ticks"))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.state.ticks.is_empty() {
        state.select(Some(app.selected_index));
    }

    frame.render_stateful_widget(list, area, &mut state);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::tick::Tick;

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
    fn test_render_with_ticks_does_not_panic() {
        let mut app = App::new();
        app.state.ticks.push(Tick::new(1));
        app.state.ticks.push(Tick::new(2));

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }
}
