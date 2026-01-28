//! Application state for the TUI.
//!
//! This module defines the core state types that drive the TUI:
//! - `AppState`: All mutable application state
//! - `View`: Which view is currently active
//! - `InteractionMode`: Current input mode (normal, chat input, help, etc.)

use super::tree::LoopTree;

/// The primary application state.
///
/// Contains all mutable state for the TUI. This is owned by `App` and
/// updated in response to events and TaskStore polling.
#[derive(Debug, Default)]
pub struct AppState {
    // View state
    /// Currently active view (Chat or Loops)
    pub current_view: View,
    /// Current interaction mode
    pub interaction_mode: InteractionMode,

    // Chat state
    /// Message history for the chat view
    pub chat_history: Vec<ChatMessage>,
    /// Current input buffer
    pub chat_input: String,
    /// Whether we're currently streaming a response
    pub chat_streaming: bool,
    /// Scroll position in chat history
    pub chat_scroll: usize,

    // Loops state
    /// Hierarchical tree of loops
    pub loops_tree: LoopTree,

    // Metrics (from TaskStore polling)
    /// Number of actively running loops
    pub loops_active: usize,
    /// Number of draft loops
    pub loops_draft: usize,
    /// Number of completed loops
    pub loops_complete: usize,
    /// Total input tokens this session
    pub session_input_tokens: u64,
    /// Total output tokens this session
    pub session_output_tokens: u64,
    /// Total cost in USD this session
    pub session_cost_usd: f64,

    // Pending actions (processed by runner)
    /// Pending chat message to submit
    pub pending_chat_submit: Option<String>,
    /// Pending action on a loop
    pub pending_action: Option<PendingAction>,

    // Control flags
    /// Whether the application should quit
    pub should_quit: bool,
}

impl AppState {
    /// Create a new default state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the status indicator character for display.
    pub fn status_indicator(&self) -> char {
        if self.loops_active > 0 {
            '●' // Running
        } else {
            '○' // Idle
        }
    }

    /// Format the header metrics string.
    pub fn metrics_string(&self) -> String {
        format!(
            "↑{} ↓{} │ ${:.2}",
            format_tokens(self.session_input_tokens),
            format_tokens(self.session_output_tokens),
            self.session_cost_usd
        )
    }

    /// Format the loop counts string.
    pub fn loop_counts_string(&self) -> String {
        format!(
            "{} active │ {} draft │ {} done",
            self.loops_active, self.loops_draft, self.loops_complete
        )
    }
}

/// Format token count with K suffix for large numbers.
fn format_tokens(count: u64) -> String {
    if count >= 1000 {
        format!("{:.1}K", count as f64 / 1000.0)
    } else {
        count.to_string()
    }
}

/// Which view is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Conversation with LLM
    #[default]
    Chat,
    /// Hierarchical tree of running loops
    Loops,
}

impl View {
    /// Cycle to the next view.
    pub fn next(self) -> Self {
        match self {
            View::Chat => View::Loops,
            View::Loops => View::Chat,
        }
    }
}

/// Current interaction mode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum InteractionMode {
    /// Normal navigation
    #[default]
    Normal,
    /// Typing in chat input
    ChatInput,
    /// Help overlay visible
    Help,
    /// Confirmation dialog
    Confirm(ConfirmDialog),
}

/// Confirmation dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmDialog {
    /// The message to display
    pub message: String,
    /// The action to take on confirm
    pub action: ConfirmAction,
}

/// Actions that require confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    /// Quit the application (with running loops)
    Quit,
    /// Cancel a loop
    CancelLoop(String),
    /// Delete a loop
    DeleteLoop(String),
}

/// A message in the chat history.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Who sent the message
    pub role: ChatRole,
    /// The message content
    pub content: String,
    /// Tool calls made (if assistant message)
    pub tool_calls: Vec<ToolCallDisplay>,
}

impl ChatMessage {
    /// Create a new user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            tool_calls: Vec::new(),
        }
    }

    /// Create a new assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
        }
    }

    /// Create a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
            tool_calls: Vec::new(),
        }
    }
}

/// Who sent a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
}

/// Tool call display information.
#[derive(Debug, Clone)]
pub struct ToolCallDisplay {
    /// Name of the tool
    pub name: String,
    /// Brief summary of the result
    pub summary: String,
    /// Whether the result is expanded
    pub expanded: bool,
    /// Full result content (if expanded)
    pub full_result: Option<String>,
}

/// Pending actions on loops.
#[derive(Debug, Clone)]
pub enum PendingAction {
    /// Cancel a loop
    CancelLoop(String),
    /// Pause a loop
    PauseLoop(String),
    /// Resume a loop
    ResumeLoop(String),
    /// Activate a draft loop
    ActivateDraft(String),
    /// Delete a loop
    DeleteLoop(String),
    /// Create a plan from description
    CreatePlan(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_view_cycle() {
        assert_eq!(View::Chat.next(), View::Loops);
        assert_eq!(View::Loops.next(), View::Chat);
    }

    #[test]
    fn test_app_state_default() {
        let state = AppState::new();
        assert_eq!(state.current_view, View::Chat);
        assert_eq!(state.interaction_mode, InteractionMode::Normal);
        assert!(state.chat_history.is_empty());
        assert!(!state.should_quit);
    }

    #[test]
    fn test_status_indicator() {
        let mut state = AppState::new();
        assert_eq!(state.status_indicator(), '○');

        state.loops_active = 1;
        assert_eq!(state.status_indicator(), '●');
    }

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1.0K");
        assert_eq!(format_tokens(1500), "1.5K");
        assert_eq!(format_tokens(10000), "10.0K");
    }

    #[test]
    fn test_metrics_string() {
        let mut state = AppState::new();
        state.session_input_tokens = 1500;
        state.session_output_tokens = 300;
        state.session_cost_usd = 0.15;

        let metrics = state.metrics_string();
        assert!(metrics.contains("1.5K"));
        assert!(metrics.contains("300"));
        assert!(metrics.contains("$0.15"));
    }

    #[test]
    fn test_chat_message_creation() {
        let user_msg = ChatMessage::user("Hello");
        assert_eq!(user_msg.role, ChatRole::User);
        assert_eq!(user_msg.content, "Hello");
        assert!(user_msg.tool_calls.is_empty());

        let assistant_msg = ChatMessage::assistant("Hi there!");
        assert_eq!(assistant_msg.role, ChatRole::Assistant);
    }
}
