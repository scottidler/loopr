use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use crate::tui::app::App;

pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .state
        .locks
        .iter()
        .map(|l| {
            let expires = l.expires_at.map(|ts| format!(", expires={ts}")).unwrap_or_default();
            ListItem::new(Line::from(format!(
                "[{}] {} holder={} granted_by={}{}",
                l.status, l.resource, l.holder_id, l.granted_by, expires
            )))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Locks"))
        .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !app.state.locks.is_empty() {
        state.select(Some(app.selected_index));
    }

    frame.render_stateful_widget(list, area, &mut state);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

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
}
