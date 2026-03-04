use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::tui::app::{App, ChatMode, ChatRole, InputMode, colors};

/// Render the chat view: history area + input area.
pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let title = match app.chat_mode {
        ChatMode::Chat => " Chat ",
        ChatMode::Plan => " Plan ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(colors::HEADER));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Split inner: history fills remaining, input at bottom (1 line)
    let input_height = 1;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),               // History
            Constraint::Length(input_height), // Input
        ])
        .split(inner);

    render_history(app, frame, chunks[0]);
    render_input(app, frame, chunks[1]);
}

/// Render chat history or welcome message.
fn render_history(app: &App, frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    if app.chat_history.is_empty() && !app.chat_streaming {
        // Welcome message
        lines.push(Line::from(Span::styled(
            "Welcome to Loopr Chat",
            Style::default().fg(colors::HEADER).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Type a message and press Enter to explore ideas.",
            Style::default().fg(colors::DIM),
        )));
        lines.push(Line::from(Span::styled(
            "Type /plan when ready to formalize a plan.",
            Style::default().fg(colors::DIM),
        )));
    } else {
        for msg in &app.chat_history {
            let (prefix, style) = match msg.role {
                ChatRole::User => (
                    "> ",
                    Style::default().fg(colors::REPL_USER).add_modifier(Modifier::BOLD),
                ),
                ChatRole::Assistant => ("  ", Style::default().fg(colors::REPL_ASSISTANT)),
                ChatRole::System => ("  ", Style::default().fg(colors::DIM)),
            };

            for line_text in msg.content.lines() {
                lines.push(Line::from(Span::styled(format!("{prefix}{line_text}"), style)));
            }
            lines.push(Line::from("")); // blank line between messages
        }

        // Show streaming response buffer if active
        if app.chat_streaming && !app.chat_response_buffer.is_empty() {
            let style = Style::default().fg(colors::REPL_ASSISTANT);
            for line_text in app.chat_response_buffer.lines() {
                lines.push(Line::from(Span::styled(format!("  {line_text}"), style)));
            }
            lines.push(Line::from("")); // blank after streaming
        }

        // Show thinking indicator while streaming
        if app.chat_streaming {
            lines.push(Line::from(Span::styled(
                "  * Thinking...",
                Style::default().fg(colors::DIM).add_modifier(Modifier::ITALIC),
            )));
        }
    }

    // Calculate scroll offset
    let total_lines = lines.len() as u16;
    let visible = area.height;
    let scroll_offset = if let Some(manual_scroll) = app.chat_scroll {
        // Manual scroll: offset from bottom
        let max_scroll = total_lines.saturating_sub(visible);
        let clamped = (manual_scroll as u16).min(max_scroll);
        max_scroll.saturating_sub(clamped)
    } else {
        // Auto-scroll: show bottom
        total_lines.saturating_sub(visible)
    };

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset, 0));

    frame.render_widget(paragraph, area);
}

/// Render the input line with cursor.
fn render_input(app: &App, frame: &mut Frame, area: Rect) {
    if area.width < 4 {
        return;
    }

    let prefix = "> ";
    let input = &app.chat_input;
    let cursor_pos = app.chat_cursor_pos;

    let (before_cursor, after_cursor) = input.split_at(cursor_pos);

    let dimmed = app.chat_streaming;
    let prefix_style = if dimmed {
        Style::default().fg(colors::DIM)
    } else {
        Style::default().fg(colors::REPL_USER).add_modifier(Modifier::BOLD)
    };
    let text_style = if dimmed { Style::default().fg(colors::DIM) } else { Style::default() };

    let mut spans = vec![
        Span::styled(prefix, prefix_style),
        Span::styled(before_cursor.to_string(), text_style),
    ];

    if app.input_mode == InputMode::ChatInput && !dimmed {
        if after_cursor.is_empty() {
            // Cursor at end: show blinking underscore
            spans.push(Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)));
        } else {
            // Cursor in middle: highlight current char
            let mut chars = after_cursor.chars();
            let cursor_char = chars.next().unwrap_or('_');
            spans.push(Span::styled(
                cursor_char.to_string(),
                Style::default()
                    .add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::SLOW_BLINK),
            ));
            let rest: String = chars.collect();
            if !rest.is_empty() {
                spans.push(Span::styled(rest, text_style));
            }
        }
    } else {
        spans.push(Span::styled(after_cursor.to_string(), text_style));
    }

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{App, ChatMessage};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn test_terminal() -> Terminal<TestBackend> {
        let backend = TestBackend::new(80, 24);
        Terminal::new(backend).unwrap()
    }

    #[test]
    fn test_render_empty_chat() {
        let app = App::new();
        let mut terminal = test_terminal();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn test_render_with_messages() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user("hello".into()));
        app.chat_history.push(ChatMessage::assistant("hi there".into()));
        app.chat_history.push(ChatMessage::system("system message".into()));

        let mut terminal = test_terminal();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn test_render_streaming() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user("hello".into()));
        app.chat_streaming = true;
        app.chat_response_buffer = "partial response...".into();

        let mut terminal = test_terminal();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn test_render_with_input() {
        let mut app = App::new();
        app.chat_input = "hello world".into();
        app.chat_cursor_pos = 5;

        let mut terminal = test_terminal();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn test_render_small_terminal() {
        let app = App::new();
        let backend = TestBackend::new(10, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn test_render_plan_mode() {
        let mut app = App::new();
        app.chat_mode = ChatMode::Plan;
        let mut terminal = test_terminal();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn test_render_scroll_mode() {
        let mut app = App::new();
        app.input_mode = InputMode::ChatScroll;
        app.chat_scroll = Some(5);
        for i in 0..50 {
            app.chat_history.push(ChatMessage::user(format!("message {i}")));
        }

        let mut terminal = test_terminal();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn test_render_plan_display_with_pending_approval() {
        let mut app = App::new();
        app.chat_mode = ChatMode::Plan;
        app.pending_plan_id = Some("plan-123".to_string());
        app.chat_history
            .push(ChatMessage::user("Let's add parallel validation".into()));
        app.chat_history.push(ChatMessage::system(
            "Entering Plan mode. Chat context sent to Coordinator.".into(),
        ));
        app.chat_history.push(ChatMessage::assistant(
            "Should parallel validation be opt-in or the default?".into(),
        ));
        app.chat_history.push(ChatMessage::user("Opt-in via config".into()));
        app.chat_history.push(ChatMessage::system(
            "=== Proposed Plan ===\nTitle: Parallel Bundle Validation\n\nPress Ctrl+a to approve and activate.".into(),
        ));

        let mut terminal = test_terminal();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }

    #[test]
    fn test_render_multiline_message() {
        let mut app = App::new();
        app.chat_history
            .push(ChatMessage::assistant("Line 1\nLine 2\nLine 3".into()));

        let mut terminal = test_terminal();
        terminal
            .draw(|frame| {
                render(&app, frame, frame.area());
            })
            .unwrap();
    }
}
