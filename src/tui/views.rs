//! TUI Views
//!
//! View components for rendering different parts of the TUI.
//! Includes chat view, loops view, and approval view.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use super::app::{AppState, ChatMessage, LoopSummary, MessageSender, PendingApproval};

/// Trait for renderable views
pub trait View {
    /// Render the view to the frame
    fn render(&self, frame: &mut Frame, area: Rect, state: &AppState);

    /// Get the view title
    fn title(&self) -> &'static str;
}

/// Chat view for interacting with Loopr
pub struct ChatView;

impl ChatView {
    /// Create a new chat view
    pub fn new() -> Self {
        Self
    }

    /// Format a chat message for display
    fn format_message(msg: &ChatMessage) -> ListItem<'_> {
        let (prefix, style) = match msg.sender {
            MessageSender::User => ("You: ", Style::default().fg(Color::Green)),
            MessageSender::Daemon => ("Loopr: ", Style::default().fg(Color::Cyan)),
            MessageSender::System => ("System: ", Style::default().fg(Color::Yellow)),
        };

        let line = Line::from(vec![
            Span::styled(prefix, style.add_modifier(Modifier::BOLD)),
            Span::raw(&msg.content),
        ]);

        ListItem::new(line)
    }
}

impl Default for ChatView {
    fn default() -> Self {
        Self::new()
    }
}

impl View for ChatView {
    fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(area);

        // Messages area
        let items: Vec<ListItem> = state.chat_messages.iter().map(Self::format_message).collect();

        let messages = List::new(items).block(Block::default().borders(Borders::ALL).title(" Chat Messages "));
        frame.render_widget(messages, chunks[0]);

        // Input area
        let input =
            Paragraph::new(state.chat_input.as_str()).block(Block::default().borders(Borders::ALL).title(" Input "));
        frame.render_widget(input, chunks[1]);
    }

    fn title(&self) -> &'static str {
        "Chat"
    }
}

/// Loops view for displaying active loops
pub struct LoopsView;

impl LoopsView {
    /// Create a new loops view
    pub fn new() -> Self {
        Self
    }

    /// Format a loop summary for display
    fn format_loop(summary: &LoopSummary, selected: bool) -> ListItem<'static> {
        let indent = "  ".repeat(summary.depth);
        let status_color = match summary.status.as_str() {
            "Running" => Color::Green,
            "Paused" => Color::Yellow,
            "Complete" => Color::Cyan,
            "Failed" => Color::Red,
            _ => Color::White,
        };

        let style = if selected {
            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        // Use owned strings to satisfy 'static lifetime
        let line = Line::from(vec![
            Span::raw(indent),
            Span::styled(format!("[{}] ", summary.loop_type), Style::default().fg(Color::Magenta)),
            Span::raw(summary.id.clone()),
            Span::raw(" - "),
            Span::styled(summary.status.clone(), Style::default().fg(status_color)),
            Span::raw(format!(" ({}/{})", summary.iteration, summary.max_iterations)),
        ]);

        ListItem::new(line).style(style)
    }
}

impl Default for LoopsView {
    fn default() -> Self {
        Self::new()
    }
}

impl View for LoopsView {
    fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
        let items: Vec<ListItem> = state
            .loops
            .iter()
            .enumerate()
            .map(|(i, l)| Self::format_loop(l, state.selected_loop == Some(i)))
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Loops ({}) ", state.loops.len())),
        );

        frame.render_widget(list, area);
    }

    fn title(&self) -> &'static str {
        "Loops"
    }
}

/// Approval view for reviewing and approving plans
pub struct ApprovalView;

impl ApprovalView {
    /// Create a new approval view
    pub fn new() -> Self {
        Self
    }

    /// Render the approval content
    fn render_approval(frame: &mut Frame, area: Rect, approval: &PendingApproval) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(5), Constraint::Length(3)])
            .split(area);

        // Plan content
        let content = Paragraph::new(approval.content.as_str())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Plan: {} ", approval.loop_id)),
            );
        frame.render_widget(content, chunks[0]);

        // Specs to be created
        let specs_text = if approval.specs.is_empty() {
            "No specs defined".to_string()
        } else {
            approval.specs.join(", ")
        };
        let specs = Paragraph::new(specs_text).block(Block::default().borders(Borders::ALL).title(" Specs to Create "));
        frame.render_widget(specs, chunks[1]);

        // Feedback input
        let feedback = Paragraph::new(approval.feedback.as_str())
            .block(Block::default().borders(Borders::ALL).title(" Feedback (optional) "));
        frame.render_widget(feedback, chunks[2]);
    }

    /// Render empty state when no approval pending
    fn render_empty(frame: &mut Frame, area: Rect) {
        let message = Paragraph::new("No plan awaiting approval")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title(" Approval "));
        frame.render_widget(message, area);
    }
}

impl Default for ApprovalView {
    fn default() -> Self {
        Self::new()
    }
}

impl View for ApprovalView {
    fn render(&self, frame: &mut Frame, area: Rect, state: &AppState) {
        match &state.pending_approval {
            Some(approval) => Self::render_approval(frame, area, approval),
            None => Self::render_empty(frame, area),
        }
    }

    fn title(&self) -> &'static str {
        "Approval"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::ActiveView;

    #[test]
    fn test_chat_view_new() {
        let view = ChatView::new();
        assert_eq!(view.title(), "Chat");
    }

    #[test]
    fn test_chat_view_default() {
        let view = ChatView::default();
        assert_eq!(view.title(), "Chat");
    }

    #[test]
    fn test_loops_view_new() {
        let view = LoopsView::new();
        assert_eq!(view.title(), "Loops");
    }

    #[test]
    fn test_loops_view_default() {
        let view = LoopsView::default();
        assert_eq!(view.title(), "Loops");
    }

    #[test]
    fn test_approval_view_new() {
        let view = ApprovalView::new();
        assert_eq!(view.title(), "Approval");
    }

    #[test]
    fn test_approval_view_default() {
        let view = ApprovalView::default();
        assert_eq!(view.title(), "Approval");
    }

    #[test]
    fn test_format_user_message() {
        let msg = ChatMessage {
            sender: MessageSender::User,
            content: "Hello".to_string(),
            timestamp: 0,
        };
        let _item = ChatView::format_message(&msg);
        // Just verify it doesn't panic
    }

    #[test]
    fn test_format_daemon_message() {
        let msg = ChatMessage {
            sender: MessageSender::Daemon,
            content: "Hi there".to_string(),
            timestamp: 0,
        };
        let _item = ChatView::format_message(&msg);
    }

    #[test]
    fn test_format_system_message() {
        let msg = ChatMessage {
            sender: MessageSender::System,
            content: "Connected".to_string(),
            timestamp: 0,
        };
        let _item = ChatView::format_message(&msg);
    }

    #[test]
    fn test_format_loop_running() {
        let summary = LoopSummary {
            id: "001".to_string(),
            loop_type: "Plan".to_string(),
            status: "Running".to_string(),
            iteration: 1,
            max_iterations: 5,
            parent_id: None,
            depth: 0,
        };
        let _item = LoopsView::format_loop(&summary, false);
    }

    #[test]
    fn test_format_loop_selected() {
        let summary = LoopSummary {
            id: "001".to_string(),
            loop_type: "Plan".to_string(),
            status: "Running".to_string(),
            iteration: 1,
            max_iterations: 5,
            parent_id: None,
            depth: 0,
        };
        let _item = LoopsView::format_loop(&summary, true);
    }

    #[test]
    fn test_format_loop_nested() {
        let summary = LoopSummary {
            id: "002".to_string(),
            loop_type: "Spec".to_string(),
            status: "Pending".to_string(),
            iteration: 0,
            max_iterations: 3,
            parent_id: Some("001".to_string()),
            depth: 2,
        };
        let _item = LoopsView::format_loop(&summary, false);
    }

    #[test]
    fn test_format_loop_statuses() {
        let statuses = ["Running", "Paused", "Complete", "Failed", "Unknown"];
        for status in statuses {
            let summary = LoopSummary {
                id: "001".to_string(),
                loop_type: "Plan".to_string(),
                status: status.to_string(),
                iteration: 1,
                max_iterations: 5,
                parent_id: None,
                depth: 0,
            };
            let _item = LoopsView::format_loop(&summary, false);
        }
    }

    #[test]
    fn test_view_trait_chat() {
        let view: &dyn View = &ChatView::new();
        assert_eq!(view.title(), "Chat");
    }

    #[test]
    fn test_view_trait_loops() {
        let view: &dyn View = &LoopsView::new();
        assert_eq!(view.title(), "Loops");
    }

    #[test]
    fn test_view_trait_approval() {
        let view: &dyn View = &ApprovalView::new();
        assert_eq!(view.title(), "Approval");
    }

    #[test]
    fn test_app_state_for_views() {
        let state = AppState {
            active_view: ActiveView::Chat,
            should_quit: false,
            chat_input: "test".to_string(),
            chat_messages: vec![],
            loops: vec![],
            selected_loop: None,
            pending_approval: None,
            status_message: None,
        };
        assert_eq!(state.active_view, ActiveView::Chat);
    }

    #[test]
    fn test_pending_approval_struct() {
        let approval = PendingApproval {
            loop_id: "001".to_string(),
            content: "# Plan".to_string(),
            specs: vec!["spec1".to_string()],
            feedback: String::new(),
        };
        assert_eq!(approval.loop_id, "001");
        assert_eq!(approval.specs.len(), 1);
    }

    #[test]
    fn test_pending_approval_empty_specs() {
        let approval = PendingApproval {
            loop_id: "001".to_string(),
            content: "# Plan".to_string(),
            specs: vec![],
            feedback: String::new(),
        };
        assert!(approval.specs.is_empty());
    }
}
