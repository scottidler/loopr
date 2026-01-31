//! View rendering for the TUI.
//!
//! This module handles all ratatui rendering for both Chat and Loops views.

use super::colors;
use super::state::{AppState, ChatRole, InteractionMode, View};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

/// Main render function - dispatches to view-specific renderers.
/// `streaming_text` is the in-progress LLM response being streamed.
pub fn render(state: &AppState, frame: &mut Frame, streaming_text: Option<&str>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Content
            Constraint::Length(3), // Footer
        ])
        .split(frame.area());

    render_header(state, frame, chunks[0]);

    match state.current_view {
        View::Chat => render_chat(state, frame, chunks[1], streaming_text),
        View::Loops => render_loops(state, frame, chunks[1]),
    }

    render_footer(state, frame, chunks[2]);

    // Render overlays
    match &state.interaction_mode {
        InteractionMode::Help => render_help_overlay(frame),
        InteractionMode::Confirm(dialog) => {
            render_confirm_overlay(frame, &dialog.message);
        }
        _ => {}
    }
}

fn render_header(state: &AppState, frame: &mut Frame, area: Rect) {
    let indicator = state.status_indicator();
    let indicator_color = if indicator == '●' { colors::RUNNING } else { colors::DIM };

    let chat_style = if state.current_view == View::Chat {
        Style::default().fg(colors::HEADER).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(colors::DIM)
    };

    let loops_style = if state.current_view == View::Loops {
        Style::default().fg(colors::HEADER).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(colors::DIM)
    };

    let left = Line::from(vec![
        Span::styled(format!(" {} ", indicator), Style::default().fg(indicator_color)),
        Span::styled(
            "Loopr",
            Style::default().fg(colors::HEADER).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" │ "),
        Span::styled("Chat", chat_style),
        Span::raw(" · "),
        Span::styled("Loops", loops_style),
    ]);

    let right = format!("{} │ {} ", state.metrics_string(), state.loop_counts_string());

    // Calculate padding for right alignment
    let left_len = left.width();
    let right_len = right.len();
    let padding = area.width.saturating_sub(left_len as u16 + right_len as u16);

    // Build header line - combine left spans with padding and right text
    let mut header_spans = left.spans.clone();
    header_spans.push(Span::raw(" ".repeat(padding as usize)));
    header_spans.push(Span::styled(right, Style::default().fg(colors::DIM)));

    let full_line = Line::from(header_spans);

    let header = Paragraph::new(full_line).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(colors::DIM)),
    );

    frame.render_widget(header, area);
}

fn render_chat(state: &AppState, frame: &mut Frame, area: Rect, streaming_text: Option<&str>) {
    // Create outer block with border
    let outer_block = Block::default()
        .title(" Chat ")
        .title_style(Style::default().fg(colors::HEADER))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colors::DIM));

    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    // Split inner area: history takes most space, input is 1 line at bottom
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),    // Chat history
            Constraint::Length(1), // Input line
        ])
        .split(inner_area);

    // Chat history
    let mut history_items: Vec<ListItem> = state
        .chat_history
        .iter()
        .flat_map(|msg| {
            let (prefix, style) = match msg.role {
                ChatRole::User => ("> ", Style::default().fg(Color::White)),
                ChatRole::Assistant => ("  ", Style::default().fg(colors::HEADER)),
                ChatRole::System => ("  ", Style::default().fg(colors::DIM).add_modifier(Modifier::ITALIC)),
            };

            // Split content into lines and prefix the first line
            let lines: Vec<Line> = msg
                .content
                .lines()
                .enumerate()
                .map(|(i, line)| {
                    if i == 0 {
                        Line::from(vec![Span::styled(prefix, style), Span::styled(line.to_string(), style)])
                    } else {
                        Line::from(vec![Span::raw("  "), Span::styled(line.to_string(), style)])
                    }
                })
                .collect();

            // Add tool calls if any
            let tool_lines: Vec<Line> = msg
                .tool_calls
                .iter()
                .map(|tc| {
                    Line::from(vec![
                        Span::styled("  ● ", Style::default().fg(colors::PENDING)),
                        Span::styled(&tc.name, Style::default().fg(colors::KEYBIND)),
                        Span::raw(" → "),
                        Span::styled(&tc.summary, Style::default().fg(colors::DIM)),
                    ])
                })
                .collect();

            let mut all_lines = lines;
            all_lines.extend(tool_lines);
            all_lines.push(Line::from("")); // Blank line between messages

            vec![ListItem::new(Text::from(all_lines))]
        })
        .collect();

    // Add streaming response if active
    if let Some(text) = streaming_text {
        let style = Style::default().fg(colors::HEADER);
        let lines: Vec<Line> = if text.is_empty() {
            // Show typing indicator when buffer is empty but streaming
            vec![Line::from(vec![
                Span::styled("  ", style),
                Span::styled("...", Style::default().fg(colors::DIM)),
            ])]
        } else {
            text.lines()
                .enumerate()
                .map(|(i, line)| {
                    if i == 0 {
                        Line::from(vec![Span::styled("  ", style), Span::styled(line.to_string(), style)])
                    } else {
                        Line::from(vec![Span::raw("  "), Span::styled(line.to_string(), style)])
                    }
                })
                .collect()
        };
        history_items.push(ListItem::new(Text::from(lines)));
    }

    let history = List::new(history_items);
    frame.render_widget(history, chunks[0]);

    // Input line at the bottom (no border, just the prompt)
    // Split input at cursor position for proper cursor rendering
    let cursor_pos = state.chat_cursor_pos;
    let (before_cursor, after_cursor) = state.chat_input.split_at(cursor_pos.min(state.chat_input.len()));

    let input_spans = if after_cursor.is_empty() {
        // Cursor at end - show blinking cursor
        vec![
            Span::styled("> ", Style::default().fg(colors::HEADER)),
            Span::styled(before_cursor, Style::default().fg(Color::White)),
            Span::styled("▌", Style::default().fg(Color::Gray)),
        ]
    } else {
        // Cursor in middle - highlight character under cursor
        let mut chars = after_cursor.chars();
        let cursor_char = chars.next().unwrap_or(' ');
        let rest: String = chars.collect();

        vec![
            Span::styled("> ", Style::default().fg(colors::HEADER)),
            Span::styled(before_cursor, Style::default().fg(Color::White)),
            Span::styled(
                cursor_char.to_string(),
                Style::default().fg(Color::Black).bg(Color::White),
            ),
            Span::styled(rest, Style::default().fg(Color::White)),
        ]
    };

    let input = Paragraph::new(Line::from(input_spans));
    frame.render_widget(input, chunks[1]);
}

fn render_loops(state: &AppState, frame: &mut Frame, area: Rect) {
    let visible_ids = state.loops_tree.visible_ids();
    let selected_id = state.loops_tree.selected_id();

    let items: Vec<ListItem> = visible_ids
        .iter()
        .map(|id| {
            let node = state.loops_tree.get_node(id).unwrap();
            let item = &node.item;

            let is_selected = selected_id == Some(id);

            // Build indent
            let indent = "  ".repeat(node.depth);

            // Build expand/collapse indicator
            let expand_char = if node.children.is_empty() {
                "─"
            } else if node.expanded {
                "▼"
            } else {
                "▶"
            };

            // Status icon with color
            let status_color = match item.status.as_str() {
                "running" => colors::RUNNING,
                "pending" => colors::PENDING,
                "complete" => colors::COMPLETE,
                "failed" => colors::FAILED,
                "paused" => colors::PENDING,
                "invalidated" => colors::DIM,
                _ => colors::DIM,
            };

            // Type prefix
            let type_prefix = match item.loop_type.as_str() {
                "plan" => "Plan",
                "spec" => "Spec",
                "phase" => "Phase",
                "ralph" => "Ralph",
                _ => "Loop",
            };

            // Progress indicator
            let progress = if item.status == "running" {
                format!(" (iter {})", item.iteration)
            } else if item.status == "complete" {
                " ✓".to_string()
            } else if item.status == "failed" {
                " ✗".to_string()
            } else {
                String::new()
            };

            let line = Line::from(vec![
                Span::raw(indent),
                Span::styled(format!("{} ", expand_char), Style::default().fg(colors::DIM)),
                Span::styled(item.status_icon(), Style::default().fg(status_color)),
                Span::raw(" "),
                Span::styled(format!("{}: ", type_prefix), Style::default().fg(colors::DIM)),
                Span::styled(&item.name, Style::default().fg(Color::White)),
                Span::styled(progress, Style::default().fg(status_color)),
            ]);

            let style = if is_selected {
                Style::default().bg(Color::DarkGray)
            } else {
                Style::default()
            };

            ListItem::new(line).style(style)
        })
        .collect();

    let title = format!(" Loops ({}) ", state.loops_tree.len());
    let list = List::new(items).block(
        Block::default()
            .title(title)
            .title_style(Style::default().fg(colors::HEADER))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(colors::DIM)),
    );

    frame.render_widget(list, area);
}

fn render_footer(state: &AppState, frame: &mut Frame, area: Rect) {
    let footer_line = match state.current_view {
        View::Chat => {
            // Claude Code-style minimal footer
            Line::from(vec![
                Span::styled("? ", Style::default().fg(colors::KEYBIND)),
                Span::styled("for shortcuts", Style::default().fg(colors::DIM)),
            ])
        }
        View::Loops => {
            // Loops view has more keys to show
            let keybinds = vec![
                ("[j/k]", "Navigate"),
                ("[h/l]", "Collapse/Expand"),
                ("[s]", "Start/Pause"),
                ("[Tab]", "Chat"),
                ("[?]", "Help"),
            ];
            let spans: Vec<Span> = keybinds
                .into_iter()
                .flat_map(|(key, action)| {
                    vec![
                        Span::styled(key, Style::default().fg(colors::KEYBIND)),
                        Span::raw(" "),
                        Span::styled(action, Style::default().fg(colors::DIM)),
                        Span::raw("  "),
                    ]
                })
                .collect();
            Line::from(spans)
        }
    };

    let footer = Paragraph::new(footer_line).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(colors::DIM)),
    );

    frame.render_widget(footer, area);
}

fn render_help_overlay(frame: &mut Frame) {
    let area = centered_rect(60, 80, frame.area());

    // Clear the area first
    frame.render_widget(Clear, area);

    let help_text = vec![
        "",
        "  GLOBAL KEYS",
        "  Tab        Switch views (Chat/Loops)",
        "  ?          Toggle this help (when input empty)",
        "  Ctrl+C     Force quit",
        "  Ctrl+D     Quit",
        "",
        "  CHAT VIEW",
        "  Enter      Send message",
        "  Esc        Clear input",
        "  PgUp/PgDn  Scroll history",
        "  Ctrl+↑/↓   Scroll one line",
        "  /plan      Create a plan",
        "  /clear     Clear conversation",
        "",
        "  LOOPS VIEW",
        "  j/k        Navigate tree",
        "  h/l        Collapse/expand",
        "  Enter      Toggle expand",
        "  s          Start/pause loop",
        "  x          Cancel loop",
        "  D          Delete loop",
        "  g/G        Top/bottom of tree",
        "  q          Quit",
        "",
        "  Press ? or Esc to close",
    ];

    let text: Vec<Line> = help_text
        .iter()
        .map(|s| Line::from(Span::styled(*s, Style::default().fg(Color::White))))
        .collect();

    let help = Paragraph::new(text).block(
        Block::default()
            .title(" Help ")
            .title_style(Style::default().fg(colors::HEADER).add_modifier(Modifier::BOLD))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(colors::HEADER)),
    );

    frame.render_widget(help, area);
}

fn render_confirm_overlay(frame: &mut Frame, message: &str) {
    let area = centered_rect(40, 20, frame.area());

    frame.render_widget(Clear, area);

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(message, Style::default().fg(Color::White))),
        Line::from(""),
        Line::from(vec![
            Span::styled("[y]", Style::default().fg(colors::KEYBIND)),
            Span::raw(" Yes  "),
            Span::styled("[n]", Style::default().fg(colors::KEYBIND)),
            Span::raw(" No"),
        ]),
    ];

    let dialog = Paragraph::new(text).wrap(Wrap { trim: true }).block(
        Block::default()
            .title(" Confirm ")
            .title_style(Style::default().fg(colors::FAILED).add_modifier(Modifier::BOLD))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(colors::FAILED)),
    );

    frame.render_widget(dialog, area);
}

/// Calculate a centered rectangle with the given percentage of width and height.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_centered_rect() {
        let area = Rect::new(0, 0, 100, 50);
        let centered = centered_rect(50, 50, area);

        // Should be roughly centered
        assert!(centered.x >= 20);
        assert!(centered.y >= 10);
        assert!(centered.width <= 60);
        assert!(centered.height <= 30);
    }
}
