use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::tui::app::{App, ChatMode, ChatRole, FunnelState, InputMode, colors};

/// Calculate the height needed for the input area based on content and width.
fn calculate_input_height(input: &str, width: u16) -> u16 {
    if input.is_empty() {
        return 1;
    }
    let effective_width = width.saturating_sub(3) as usize; // "> " prefix + cursor
    if effective_width == 0 {
        return 1;
    }
    input
        .split('\n')
        .map(|seg| (seg.len().div_ceil(effective_width)).max(1))
        .sum::<usize>()
        .max(1) as u16
}

/// Render the chat view: history area + input area.
pub fn render(app: &App, frame: &mut Frame, area: Rect) {
    let title = match app.chat_mode {
        ChatMode::Chat => " Chat ",
        ChatMode::Plan => " Plan ",
    };

    let border_color = match app.funnel_state {
        FunnelState::Chat => colors::HEADER,
        FunnelState::Interview => Color::Yellow,
        FunnelState::PlanDraft => Color::Green,
        FunnelState::Executing => Color::Blue,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let input_height = calculate_input_height(&app.chat_input, inner.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),               // History
            Constraint::Length(input_height), // Input (dynamic)
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
                ChatRole::ToolInvocation => ("  ", Style::default().fg(colors::DIM).add_modifier(Modifier::ITALIC)),
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

        // Show animated thinking indicator while streaming
        if app.chat_streaming {
            let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let spinner = spinner_chars[(app.frame_count as usize / 3) % spinner_chars.len()];
            lines.push(Line::from(Span::styled(
                format!("  {spinner} Thinking..."),
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
    let paragraph = Paragraph::new(line).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{App, ChatMessage};
    use crate::tui::test_utils::{buffer_contains_text, test_terminal};

    #[test]
    fn test_render_empty_chat_shows_welcome() {
        let app = App::new();
        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Welcome to Loopr Chat"));
        assert!(buffer_contains_text(buffer, "Type a message and press Enter"));
        assert!(buffer_contains_text(buffer, "/plan"));
    }

    #[test]
    fn test_render_user_message_with_prefix() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user("hello world".into()));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "> hello world"));
        // Welcome message should be gone
        assert!(!buffer_contains_text(buffer, "Welcome to Loopr Chat"));
    }

    #[test]
    fn test_render_assistant_message() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user("hello".into()));
        app.chat_history.push(ChatMessage::assistant("hi there".into()));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "> hello"));
        assert!(buffer_contains_text(buffer, "hi there"));
    }

    #[test]
    fn test_render_system_message() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::system("system message".into()));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "system message"));
    }

    #[test]
    fn test_render_tool_invocation() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user("do something".into()));
        app.chat_history.push(ChatMessage::tool_invocation("bash", false));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "tool: bash"));
    }

    #[test]
    fn test_render_streaming_shows_thinking() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user("hello".into()));
        app.chat_streaming = true;
        app.chat_response_buffer = "partial response...".into();

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "partial response..."));
        assert!(buffer_contains_text(buffer, "Thinking..."));
    }

    #[test]
    fn test_render_streaming_empty_buffer_shows_thinking() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user("hello".into()));
        app.chat_streaming = true;

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Thinking..."));
    }

    #[test]
    fn test_render_with_input_text() {
        let mut app = App::new();
        app.chat_input = "hello world".into();
        app.chat_cursor_pos = 5;

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        // Input area shows the "> " prefix and the typed text
        assert!(buffer_contains_text(buffer, "> hello world"));
    }

    #[test]
    fn test_render_small_terminal() {
        let app = App::new();
        let mut terminal = test_terminal(10, 5);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();
        // No panic is the primary assertion; small terminals may truncate content
    }

    #[test]
    fn test_render_chat_mode_title() {
        let mut app = App::new();
        let mut terminal = test_terminal(80, 24);

        // Chat mode shows " Chat " title
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Chat"));

        // Plan mode shows " Plan " title
        app.chat_mode = ChatMode::Plan;
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Plan"));
    }

    #[test]
    fn test_render_scroll_mode() {
        let mut app = App::new();
        app.input_mode = InputMode::ChatScroll;
        app.chat_scroll = Some(5);
        for i in 0..50 {
            app.chat_history.push(ChatMessage::user(format!("message {i}")));
        }

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        // With scroll offset, we should see earlier messages, not the latest ones
        // message 49 is at the bottom; scrolling up 5 should hide it
        assert!(!buffer_contains_text(buffer, "> message 49"));
    }

    #[test]
    fn test_render_plan_display_with_pending_approval() {
        let mut app = App::new();
        app.chat_mode = ChatMode::Plan;
        app.funnel_state = FunnelState::PlanDraft;
        app.chat_history
            .push(ChatMessage::user("Let's add parallel validation".into()));
        app.chat_history
            .push(ChatMessage::system("Entering Plan mode. Focusing on your goal.".into()));
        app.chat_history.push(ChatMessage::assistant(
            "Should parallel validation be opt-in or the default?".into(),
        ));
        app.chat_history.push(ChatMessage::user("Opt-in via config".into()));
        app.chat_history.push(ChatMessage::assistant(
            "Title: Parallel Bundle Validation\n\nGoal: Add parallel validation\n\nAcceptance Criteria:\n1. Opt-in via config"
                .into(),
        ));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Parallel Bundle Validation"));
        assert!(buffer_contains_text(buffer, "Opt-in via config"));
    }

    #[test]
    fn test_render_multiline_message() {
        let mut app = App::new();
        app.chat_history
            .push(ChatMessage::assistant("Line 1\nLine 2\nLine 3".into()));

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Line 1"));
        assert!(buffer_contains_text(buffer, "Line 2"));
        assert!(buffer_contains_text(buffer, "Line 3"));
    }

    #[test]
    fn test_calculate_input_height_empty() {
        assert_eq!(calculate_input_height("", 80), 1);
    }

    #[test]
    fn test_calculate_input_height_single_line() {
        assert_eq!(calculate_input_height("hello", 80), 1);
    }

    #[test]
    fn test_calculate_input_height_multiline() {
        assert_eq!(calculate_input_height("line1\nline2\nline3", 80), 3);
    }

    #[test]
    fn test_calculate_input_height_wrapping() {
        // width=10, effective=7, "abcdefghij" = 10 chars -> ceil(10/7) = 2
        assert_eq!(calculate_input_height("abcdefghij", 10), 2);
    }

    #[test]
    fn test_calculate_input_height_zero_width() {
        assert_eq!(calculate_input_height("hello", 0), 1);
        assert_eq!(calculate_input_height("hello", 3), 1); // effective=0
    }

    #[test]
    fn test_render_with_multiline_input() {
        let mut app = App::new();
        app.chat_input = "line1\nline2".into();
        app.chat_cursor_pos = 11;

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "line1"));
    }

    #[test]
    fn test_render_funnel_state_colors() {
        let mut app = App::new();
        let mut terminal = test_terminal(80, 24);

        // Chat state
        app.funnel_state = FunnelState::Chat;
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        // Interview state
        app.funnel_state = FunnelState::Interview;
        app.chat_mode = ChatMode::Plan;
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        // PlanDraft state
        app.funnel_state = FunnelState::PlanDraft;
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        // Executing state
        app.funnel_state = FunnelState::Executing;
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();
    }

    // --- Terminal size resilience tests ---

    #[test]
    fn test_render_various_terminal_sizes() {
        let sizes = [(80, 24), (120, 40), (40, 10), (20, 6)];
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user("hello".into()));
        app.chat_history.push(ChatMessage::assistant("world".into()));

        for (w, h) in sizes {
            let mut terminal = test_terminal(w, h);
            terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();
        }
    }

    #[test]
    fn test_render_wide_terminal_shows_full_content() {
        let mut app = App::new();
        app.chat_history
            .push(ChatMessage::user("hello from the wide terminal".into()));

        let mut terminal = test_terminal(120, 40);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "> hello from the wide terminal"));
    }

    #[test]
    fn test_render_narrow_terminal_no_panic() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user(
            "this is a longer message that will need wrapping".into(),
        ));
        app.chat_streaming = true;
        app.chat_response_buffer = "streaming content here".into();

        let mut terminal = test_terminal(30, 8);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();
        // Primary assertion: no panic. Content may be wrapped/truncated.
    }

    #[test]
    fn test_render_minimal_terminal() {
        // Absolute minimum: border takes 2 cols + 2 rows
        let app = App::new();
        let mut terminal = test_terminal(4, 4);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();
    }

    // --- Combined L2+L3 tests (replay then render) ---

    use crate::tui::test_utils::type_and_submit;

    #[test]
    fn test_replay_submit_then_render_shows_message() {
        let mut app = App::new();
        type_and_submit(&mut app, "hello world");

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "> hello world"));
        assert!(!buffer_contains_text(buffer, "Welcome to Loopr Chat"));
    }

    #[test]
    fn test_replay_plan_mode_changes_title() {
        let mut app = App::new();
        type_and_submit(&mut app, "/plan");

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Plan"));
        assert!(buffer_contains_text(buffer, "Plan mode"));
    }

    #[test]
    fn test_replay_multiple_messages_render() {
        let mut app = App::new();
        type_and_submit(&mut app, "first message");
        // Simulate assistant response
        app.chat_history.push(ChatMessage::assistant("first reply".into()));
        type_and_submit(&mut app, "second message");

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "> first message"));
        assert!(buffer_contains_text(buffer, "first reply"));
        assert!(buffer_contains_text(buffer, "> second message"));
    }

    #[test]
    fn test_replay_clear_then_render_shows_welcome() {
        let mut app = App::new();
        type_and_submit(&mut app, "hello");
        type_and_submit(&mut app, "/clear");

        let mut terminal = test_terminal(80, 24);
        terminal.draw(|frame| render(&app, frame, frame.area())).unwrap();

        let buffer = terminal.backend().buffer();
        assert!(buffer_contains_text(buffer, "Welcome to Loopr Chat"));
    }
}
