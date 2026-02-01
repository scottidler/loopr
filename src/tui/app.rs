//! TUI Application
//!
//! Main application struct that manages views, state, and event loop.

use crate::error::Result;
use crate::ipc::{IpcClient, IpcClientConfig};

/// Active view in the TUI
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActiveView {
    #[default]
    Chat,
    Loops,
    Approval,
}

impl ActiveView {
    /// Cycle to the next view
    pub fn next(self) -> Self {
        match self {
            Self::Chat => Self::Loops,
            Self::Loops => Self::Approval,
            Self::Approval => Self::Chat,
        }
    }

    /// Cycle to the previous view
    pub fn prev(self) -> Self {
        match self {
            Self::Chat => Self::Approval,
            Self::Loops => Self::Chat,
            Self::Approval => Self::Loops,
        }
    }

    /// Get the view name for display
    pub fn name(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Loops => "Loops",
            Self::Approval => "Approval",
        }
    }
}

/// Application configuration
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Path to daemon socket
    pub socket_path: std::path::PathBuf,
    /// Tick rate in milliseconds
    pub tick_rate_ms: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            socket_path: std::path::PathBuf::from("/tmp/loopr.sock"),
            tick_rate_ms: 100,
        }
    }
}

/// Application state
#[derive(Debug, Default)]
pub struct AppState {
    /// Current active view
    pub active_view: ActiveView,
    /// Whether the app should quit
    pub should_quit: bool,
    /// Chat input buffer
    pub chat_input: String,
    /// Chat message history
    pub chat_messages: Vec<ChatMessage>,
    /// Loop list for loops view
    pub loops: Vec<LoopSummary>,
    /// Currently selected loop index
    pub selected_loop: Option<usize>,
    /// Plan awaiting approval (if any)
    pub pending_approval: Option<PendingApproval>,
    /// Status message to display
    pub status_message: Option<String>,
}

/// A chat message
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Who sent the message
    pub sender: MessageSender,
    /// Message content
    pub content: String,
    /// Timestamp
    pub timestamp: i64,
}

/// Message sender type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageSender {
    User,
    System,
    Daemon,
}

/// Summary of a loop for display
#[derive(Debug, Clone)]
pub struct LoopSummary {
    /// Loop ID
    pub id: String,
    /// Loop type
    pub loop_type: String,
    /// Current status
    pub status: String,
    /// Current iteration
    pub iteration: u32,
    /// Max iterations
    pub max_iterations: u32,
    /// Parent ID if any
    pub parent_id: Option<String>,
    /// Nesting depth for tree display
    pub depth: usize,
}

/// A plan awaiting user approval
#[derive(Debug, Clone)]
pub struct PendingApproval {
    /// Plan loop ID
    pub loop_id: String,
    /// Plan content (markdown)
    pub content: String,
    /// Specs that will be created
    pub specs: Vec<String>,
    /// Feedback input buffer
    pub feedback: String,
}

/// Main TUI application
pub struct App {
    /// Application state
    pub state: AppState,
    /// Application config
    pub config: AppConfig,
    /// IPC client (optional, may not be connected)
    client: Option<IpcClient>,
}

impl App {
    /// Create a new application
    pub fn new(config: AppConfig) -> Self {
        Self {
            state: AppState::default(),
            config,
            client: None,
        }
    }

    /// Create with default config
    pub fn with_defaults() -> Self {
        Self::new(AppConfig::default())
    }

    /// Check if connected to daemon
    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    /// Connect to daemon
    pub async fn connect(&mut self) -> Result<()> {
        let config = IpcClientConfig::with_socket(&self.config.socket_path);
        let client = IpcClient::new(config);
        client.connect().await?;
        self.client = Some(client);
        self.state.status_message = Some("Connected to daemon".to_string());
        Ok(())
    }

    /// Disconnect from daemon
    pub async fn disconnect(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = client.disconnect().await;
        }
        self.state.status_message = Some("Disconnected".to_string());
    }

    /// Get mutable reference to client
    pub fn client_mut(&mut self) -> Option<&mut IpcClient> {
        self.client.as_mut()
    }

    /// Switch to next view
    pub fn next_view(&mut self) {
        self.state.active_view = self.state.active_view.next();
    }

    /// Switch to previous view
    pub fn prev_view(&mut self) {
        self.state.active_view = self.state.active_view.prev();
    }

    /// Switch to specific view
    pub fn set_view(&mut self, view: ActiveView) {
        self.state.active_view = view;
    }

    /// Request to quit
    pub fn quit(&mut self) {
        self.state.should_quit = true;
    }

    /// Add a chat message
    pub fn add_chat_message(&mut self, sender: MessageSender, content: String) {
        self.state.chat_messages.push(ChatMessage {
            sender,
            content,
            timestamp: crate::id::now_ms(),
        });
    }

    /// Set status message
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.state.status_message = Some(message.into());
    }

    /// Clear status message
    pub fn clear_status(&mut self) {
        self.state.status_message = None;
    }

    /// Update loops list
    pub fn update_loops(&mut self, loops: Vec<LoopSummary>) {
        self.state.loops = loops;
        // Reset selection if out of bounds
        if let Some(idx) = self.state.selected_loop
            && idx >= self.state.loops.len()
        {
            self.state.selected_loop = if self.state.loops.is_empty() {
                None
            } else {
                Some(self.state.loops.len() - 1)
            };
        }
    }

    /// Select next loop in list
    pub fn select_next_loop(&mut self) {
        if self.state.loops.is_empty() {
            return;
        }
        self.state.selected_loop = Some(match self.state.selected_loop {
            None => 0,
            Some(i) if i + 1 >= self.state.loops.len() => 0,
            Some(i) => i + 1,
        });
    }

    /// Select previous loop in list
    pub fn select_prev_loop(&mut self) {
        if self.state.loops.is_empty() {
            return;
        }
        self.state.selected_loop = Some(match self.state.selected_loop {
            None => self.state.loops.len() - 1,
            Some(0) => self.state.loops.len() - 1,
            Some(i) => i - 1,
        });
    }

    /// Get currently selected loop
    pub fn selected_loop(&self) -> Option<&LoopSummary> {
        self.state.selected_loop.and_then(|i| self.state.loops.get(i))
    }

    /// Set pending approval
    pub fn set_pending_approval(&mut self, approval: PendingApproval) {
        self.state.pending_approval = Some(approval);
        self.state.active_view = ActiveView::Approval;
    }

    /// Clear pending approval
    pub fn clear_pending_approval(&mut self) {
        self.state.pending_approval = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_active_view_cycle() {
        let view = ActiveView::Chat;
        assert_eq!(view.next(), ActiveView::Loops);
        assert_eq!(view.next().next(), ActiveView::Approval);
        assert_eq!(view.next().next().next(), ActiveView::Chat);
    }

    #[test]
    fn test_active_view_prev() {
        let view = ActiveView::Chat;
        assert_eq!(view.prev(), ActiveView::Approval);
        assert_eq!(view.prev().prev(), ActiveView::Loops);
        assert_eq!(view.prev().prev().prev(), ActiveView::Chat);
    }

    #[test]
    fn test_active_view_names() {
        assert_eq!(ActiveView::Chat.name(), "Chat");
        assert_eq!(ActiveView::Loops.name(), "Loops");
        assert_eq!(ActiveView::Approval.name(), "Approval");
    }

    #[test]
    fn test_app_creation() {
        let app = App::with_defaults();
        assert!(!app.is_connected());
        assert_eq!(app.state.active_view, ActiveView::Chat);
        assert!(!app.state.should_quit);
    }

    #[test]
    fn test_app_view_switching() {
        let mut app = App::with_defaults();
        assert_eq!(app.state.active_view, ActiveView::Chat);

        app.next_view();
        assert_eq!(app.state.active_view, ActiveView::Loops);

        app.next_view();
        assert_eq!(app.state.active_view, ActiveView::Approval);

        app.prev_view();
        assert_eq!(app.state.active_view, ActiveView::Loops);

        app.set_view(ActiveView::Chat);
        assert_eq!(app.state.active_view, ActiveView::Chat);
    }

    #[test]
    fn test_app_quit() {
        let mut app = App::with_defaults();
        assert!(!app.state.should_quit);
        app.quit();
        assert!(app.state.should_quit);
    }

    #[test]
    fn test_chat_messages() {
        let mut app = App::with_defaults();
        assert!(app.state.chat_messages.is_empty());

        app.add_chat_message(MessageSender::User, "Hello".to_string());
        assert_eq!(app.state.chat_messages.len(), 1);
        assert_eq!(app.state.chat_messages[0].content, "Hello");
        assert_eq!(app.state.chat_messages[0].sender, MessageSender::User);

        app.add_chat_message(MessageSender::Daemon, "Hi there".to_string());
        assert_eq!(app.state.chat_messages.len(), 2);
    }

    #[test]
    fn test_status_message() {
        let mut app = App::with_defaults();
        assert!(app.state.status_message.is_none());

        app.set_status("Testing");
        assert_eq!(app.state.status_message, Some("Testing".to_string()));

        app.clear_status();
        assert!(app.state.status_message.is_none());
    }

    #[test]
    fn test_loop_selection_empty() {
        let mut app = App::with_defaults();
        assert!(app.state.loops.is_empty());
        assert!(app.state.selected_loop.is_none());

        // Should not crash on empty list
        app.select_next_loop();
        app.select_prev_loop();
        assert!(app.state.selected_loop.is_none());
    }

    #[test]
    fn test_loop_selection() {
        let mut app = App::with_defaults();
        app.update_loops(vec![
            LoopSummary {
                id: "001".to_string(),
                loop_type: "Plan".to_string(),
                status: "Running".to_string(),
                iteration: 1,
                max_iterations: 5,
                parent_id: None,
                depth: 0,
            },
            LoopSummary {
                id: "002".to_string(),
                loop_type: "Spec".to_string(),
                status: "Pending".to_string(),
                iteration: 0,
                max_iterations: 5,
                parent_id: Some("001".to_string()),
                depth: 1,
            },
        ]);

        assert!(app.state.selected_loop.is_none());

        app.select_next_loop();
        assert_eq!(app.state.selected_loop, Some(0));

        app.select_next_loop();
        assert_eq!(app.state.selected_loop, Some(1));

        app.select_next_loop(); // Wraps to 0
        assert_eq!(app.state.selected_loop, Some(0));

        app.select_prev_loop(); // Wraps to end
        assert_eq!(app.state.selected_loop, Some(1));
    }

    #[test]
    fn test_selected_loop() {
        let mut app = App::with_defaults();
        assert!(app.selected_loop().is_none());

        app.update_loops(vec![LoopSummary {
            id: "001".to_string(),
            loop_type: "Plan".to_string(),
            status: "Running".to_string(),
            iteration: 1,
            max_iterations: 5,
            parent_id: None,
            depth: 0,
        }]);

        app.select_next_loop();
        let selected = app.selected_loop().unwrap();
        assert_eq!(selected.id, "001");
    }

    #[test]
    fn test_pending_approval() {
        let mut app = App::with_defaults();
        assert!(app.state.pending_approval.is_none());

        let approval = PendingApproval {
            loop_id: "001".to_string(),
            content: "# Plan\n\nContent here".to_string(),
            specs: vec!["spec-auth".to_string()],
            feedback: String::new(),
        };

        app.set_pending_approval(approval);
        assert!(app.state.pending_approval.is_some());
        assert_eq!(app.state.active_view, ActiveView::Approval);

        app.clear_pending_approval();
        assert!(app.state.pending_approval.is_none());
    }

    #[test]
    fn test_update_loops_resets_selection() {
        let mut app = App::with_defaults();
        app.update_loops(vec![
            LoopSummary {
                id: "001".to_string(),
                loop_type: "Plan".to_string(),
                status: "Running".to_string(),
                iteration: 1,
                max_iterations: 5,
                parent_id: None,
                depth: 0,
            },
            LoopSummary {
                id: "002".to_string(),
                loop_type: "Spec".to_string(),
                status: "Pending".to_string(),
                iteration: 0,
                max_iterations: 5,
                parent_id: Some("001".to_string()),
                depth: 1,
            },
        ]);

        app.state.selected_loop = Some(1);

        // Update with fewer loops - selection should adjust
        app.update_loops(vec![LoopSummary {
            id: "001".to_string(),
            loop_type: "Plan".to_string(),
            status: "Running".to_string(),
            iteration: 1,
            max_iterations: 5,
            parent_id: None,
            depth: 0,
        }]);

        assert_eq!(app.state.selected_loop, Some(0));

        // Update with empty - selection should clear
        app.update_loops(vec![]);
        assert!(app.state.selected_loop.is_none());
    }

    #[test]
    fn test_app_config_default() {
        let config = AppConfig::default();
        assert_eq!(config.socket_path, std::path::PathBuf::from("/tmp/loopr.sock"));
        assert_eq!(config.tick_rate_ms, 100);
    }
}
