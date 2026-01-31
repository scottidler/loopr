//! Application struct and keyboard handling.
//!
//! The `App` struct owns the `AppState` and handles keyboard input,
//! translating key presses into state changes and pending actions.

use super::state::{AppState, ChatMessage, ConfirmAction, ConfirmDialog, InteractionMode, PendingAction, View};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Main application struct that handles input and state transitions.
pub struct App {
    /// The application state
    state: AppState,
}

impl App {
    /// Create a new application with default state.
    pub fn new() -> Self {
        let mut state = AppState::new();
        // Add welcome message
        state.chat_history.push(ChatMessage::system(
            "Welcome to Loopr Chat\n\nType a message and press Enter to chat with the AI assistant.\nUse /plan <description> to create a plan.",
        ));
        // Start in chat input mode
        state.interaction_mode = InteractionMode::ChatInput;
        Self { state }
    }

    /// Get a reference to the state.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Get a mutable reference to the state.
    pub fn state_mut(&mut self) -> &mut AppState {
        &mut self.state
    }

    /// Handle a key event.
    ///
    /// Returns `true` if the application should quit.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Check for global quit keys first
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            // Ctrl+C: force quit
            return true;
        }

        // Handle based on current interaction mode
        match &self.state.interaction_mode {
            InteractionMode::Normal => self.handle_normal_mode(key),
            InteractionMode::ChatInput => self.handle_chat_input(key),
            InteractionMode::Help => self.handle_help_mode(key),
            InteractionMode::Confirm(_) => self.handle_confirm_mode(key),
        }

        self.state.should_quit
    }

    fn handle_normal_mode(&mut self, key: KeyEvent) {
        // In Chat view, automatically switch to ChatInput mode (no vim-style modes)
        if self.state.current_view == View::Chat {
            self.state.interaction_mode = InteractionMode::ChatInput;
            self.handle_chat_input(key);
            return;
        }

        match key.code {
            // Global keys
            KeyCode::Char('q') => {
                if self.state.loops_active > 0 {
                    // Confirm quit with running loops
                    self.state.interaction_mode = InteractionMode::Confirm(ConfirmDialog {
                        message: "Loops are running. Quit anyway?".to_string(),
                        action: ConfirmAction::Quit,
                    });
                } else {
                    self.state.should_quit = true;
                }
            }
            KeyCode::Tab => {
                self.state.current_view = self.state.current_view.next();
                // Auto-enter ChatInput when switching to Chat view
                if self.state.current_view == View::Chat {
                    self.state.interaction_mode = InteractionMode::ChatInput;
                }
            }
            KeyCode::Char('?') => {
                self.state.interaction_mode = InteractionMode::Help;
            }
            KeyCode::Esc => {
                // In normal mode, Esc does nothing special
            }

            // View-specific keys (only Loops now, since Chat auto-switches to ChatInput)
            _ => self.handle_loops_normal(key),
        }
    }

    fn handle_loops_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.state.loops_tree.select_next();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.state.loops_tree.select_previous();
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.state.loops_tree.collapse();
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.state.loops_tree.expand();
            }
            KeyCode::Enter => {
                // Toggle expand or describe selected loop
                self.state.loops_tree.toggle_expand();
            }
            KeyCode::Char('g') => {
                self.state.loops_tree.select_first();
            }
            KeyCode::Char('G') => {
                self.state.loops_tree.select_last();
            }
            KeyCode::Char('s') => {
                // Start/pause loop
                if let Some(id) = self.state.loops_tree.selected_id().cloned()
                    && let Some(node) = self.state.loops_tree.get_node(&id)
                {
                    let action = match node.item.status.as_str() {
                        "pending" | "paused" => Some(PendingAction::ResumeLoop(id)),
                        "running" => Some(PendingAction::PauseLoop(id)),
                        _ => None,
                    };
                    if let Some(action) = action {
                        self.state.pending_action = Some(action);
                    }
                }
            }
            KeyCode::Char('x') => {
                // Cancel loop
                if let Some(id) = self.state.loops_tree.selected_id().cloned() {
                    self.state.interaction_mode = InteractionMode::Confirm(ConfirmDialog {
                        message: "Cancel this loop?".to_string(),
                        action: ConfirmAction::CancelLoop(id),
                    });
                }
            }
            KeyCode::Char('D') => {
                // Delete loop
                if let Some(id) = self.state.loops_tree.selected_id().cloned() {
                    self.state.interaction_mode = InteractionMode::Confirm(ConfirmDialog {
                        message: "Delete this loop?".to_string(),
                        action: ConfirmAction::DeleteLoop(id),
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_chat_input(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let input = self.state.chat_input.trim().to_string();
                if !input.is_empty() {
                    // Check for commands
                    if input.starts_with("/plan ") {
                        let description = input.strip_prefix("/plan ").unwrap().trim();
                        if !description.is_empty() {
                            self.state.pending_action = Some(PendingAction::CreatePlan(description.to_string()));
                        }
                    } else if input == "/clear" {
                        self.state.chat_history.clear();
                        self.state.chat_history.push(ChatMessage::system(
                            "Chat cleared.\n\nType a message and press Enter to start a new conversation.",
                        ));
                    } else {
                        // Regular message
                        self.state.pending_chat_submit = Some(input);
                    }
                    self.state.chat_input.clear();
                    self.state.chat_cursor_pos = 0;
                }
            }
            KeyCode::Esc => {
                // Clear input on Esc (like Claude Code), don't change mode
                self.state.chat_input.clear();
                self.state.chat_cursor_pos = 0;
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+D quits (like shell EOF)
                self.state.should_quit = true;
            }
            // Cursor movement
            KeyCode::Left => {
                if self.state.chat_cursor_pos > 0 {
                    self.state.chat_cursor_pos = self.prev_char_boundary(self.state.chat_cursor_pos);
                }
            }
            KeyCode::Right => {
                if self.state.chat_cursor_pos < self.state.chat_input.len() {
                    self.state.chat_cursor_pos = self.next_char_boundary(self.state.chat_cursor_pos);
                }
            }
            KeyCode::Home => {
                self.state.chat_cursor_pos = 0;
            }
            KeyCode::End => {
                self.state.chat_cursor_pos = self.state.chat_input.len();
            }
            // Deletion
            KeyCode::Backspace => {
                if self.state.chat_cursor_pos > 0 {
                    let new_pos = self.prev_char_boundary(self.state.chat_cursor_pos);
                    self.state.chat_input.drain(new_pos..self.state.chat_cursor_pos);
                    self.state.chat_cursor_pos = new_pos;
                }
            }
            KeyCode::Delete => {
                if self.state.chat_cursor_pos < self.state.chat_input.len() {
                    let end_pos = self.next_char_boundary(self.state.chat_cursor_pos);
                    self.state.chat_input.drain(self.state.chat_cursor_pos..end_pos);
                }
            }
            KeyCode::Char('?') if self.state.chat_input.is_empty() => {
                // Show help only if input is empty
                self.state.interaction_mode = InteractionMode::Help;
            }
            KeyCode::Char(c) => {
                // Insert at cursor position
                self.state.chat_input.insert(self.state.chat_cursor_pos, c);
                self.state.chat_cursor_pos += c.len_utf8();
            }
            KeyCode::Tab => {
                // Switch views - go to Normal mode only for Loops
                self.state.current_view = self.state.current_view.next();
                if self.state.current_view == View::Loops {
                    self.state.interaction_mode = InteractionMode::Normal;
                }
            }
            // Scrolling while in input mode (no need to switch modes)
            KeyCode::PageUp => {
                self.state.chat_scroll = self.state.chat_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.state.chat_scroll = self.state.chat_scroll.saturating_add(10);
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.chat_scroll = self.state.chat_scroll.saturating_sub(1);
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.chat_scroll = self.state.chat_scroll.saturating_add(1);
            }
            _ => {}
        }
    }

    /// Get the previous character boundary for UTF-8 safe cursor movement
    fn prev_char_boundary(&self, pos: usize) -> usize {
        let input = &self.state.chat_input;
        let mut new_pos = pos.saturating_sub(1);
        while new_pos > 0 && !input.is_char_boundary(new_pos) {
            new_pos -= 1;
        }
        new_pos
    }

    /// Get the next character boundary for UTF-8 safe cursor movement
    fn next_char_boundary(&self, pos: usize) -> usize {
        let input = &self.state.chat_input;
        let mut new_pos = pos + 1;
        while new_pos < input.len() && !input.is_char_boundary(new_pos) {
            new_pos += 1;
        }
        new_pos.min(input.len())
    }

    fn handle_help_mode(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {
                // Return to ChatInput if in Chat view, Normal otherwise
                self.state.interaction_mode = if self.state.current_view == View::Chat {
                    InteractionMode::ChatInput
                } else {
                    InteractionMode::Normal
                };
            }
            _ => {}
        }
    }

    fn handle_confirm_mode(&mut self, key: KeyEvent) {
        // Clone the dialog to avoid borrow issues
        let dialog = match &self.state.interaction_mode {
            InteractionMode::Confirm(d) => d.clone(),
            _ => return,
        };

        // Determine the mode to return to
        let return_mode = if self.state.current_view == View::Chat {
            InteractionMode::ChatInput
        } else {
            InteractionMode::Normal
        };

        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                // Confirm action
                match dialog.action {
                    ConfirmAction::Quit => {
                        self.state.should_quit = true;
                    }
                    ConfirmAction::CancelLoop(id) => {
                        self.state.pending_action = Some(PendingAction::CancelLoop(id));
                    }
                    ConfirmAction::DeleteLoop(id) => {
                        self.state.pending_action = Some(PendingAction::DeleteLoop(id));
                    }
                }
                self.state.interaction_mode = return_mode;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                // Cancel
                self.state.interaction_mode = return_mode;
            }
            _ => {}
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn test_app_new() {
        let app = App::new();
        assert_eq!(app.state().current_view, View::Chat);
        assert!(!app.state().chat_history.is_empty()); // Welcome message
        assert!(matches!(app.state().interaction_mode, InteractionMode::ChatInput));
    }

    #[test]
    fn test_view_switching() {
        let mut app = App::new();
        // App starts in ChatInput mode
        assert!(matches!(app.state().interaction_mode, InteractionMode::ChatInput));
        assert_eq!(app.state().current_view, View::Chat);

        // Tab switches to Loops (and Normal mode)
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.state().current_view, View::Loops);
        assert!(matches!(app.state().interaction_mode, InteractionMode::Normal));

        // Tab back to Chat (automatically enters ChatInput mode)
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.state().current_view, View::Chat);
        assert!(matches!(app.state().interaction_mode, InteractionMode::ChatInput));
    }

    #[test]
    fn test_quit_from_loops() {
        let mut app = App::new();
        // Switch to Loops view first (where quit works)
        app.state_mut().current_view = View::Loops;
        app.state_mut().interaction_mode = InteractionMode::Normal;

        let should_quit = app.handle_key(key(KeyCode::Char('q')));
        assert!(should_quit || app.state().should_quit);
    }

    #[test]
    fn test_quit_with_loops_confirms() {
        let mut app = App::new();
        // Switch to Loops view (where quit works)
        app.state_mut().current_view = View::Loops;
        app.state_mut().interaction_mode = InteractionMode::Normal;
        app.state_mut().loops_active = 1;

        app.handle_key(key(KeyCode::Char('q')));
        assert!(matches!(app.state().interaction_mode, InteractionMode::Confirm(_)));
    }

    #[test]
    fn test_help_toggle_in_chat() {
        let mut app = App::new();
        // App starts in ChatInput mode with empty input
        assert!(matches!(app.state().interaction_mode, InteractionMode::ChatInput));
        assert!(app.state().chat_input.is_empty());

        // ? shows help when input is empty
        app.handle_key(key(KeyCode::Char('?')));
        assert!(matches!(app.state().interaction_mode, InteractionMode::Help));

        // Closing help returns to ChatInput (not Normal)
        app.handle_key(key(KeyCode::Esc));
        assert!(matches!(app.state().interaction_mode, InteractionMode::ChatInput));
    }

    #[test]
    fn test_question_mark_types_when_input_not_empty() {
        let mut app = App::new();
        app.state_mut().chat_input = "hello".to_string();
        app.state_mut().chat_cursor_pos = 5; // Cursor at end

        // ? should type into input, not show help
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.state().chat_input, "hello?");
        assert!(matches!(app.state().interaction_mode, InteractionMode::ChatInput));
    }

    #[test]
    fn test_chat_input() {
        let mut app = App::new();
        // App starts in ChatInput mode

        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.state().chat_input, "hi");

        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.state().chat_input, "h");
    }

    #[test]
    fn test_esc_clears_input() {
        let mut app = App::new();
        app.state_mut().chat_input = "some text".to_string();

        // Esc should clear input, not change mode
        app.handle_key(key(KeyCode::Esc));
        assert!(app.state().chat_input.is_empty());
        assert!(matches!(app.state().interaction_mode, InteractionMode::ChatInput));
    }

    #[test]
    fn test_chat_submit() {
        let mut app = App::new();
        app.state_mut().chat_input = "hello".to_string();

        app.handle_key(key(KeyCode::Enter));

        assert!(app.state().pending_chat_submit.is_some());
        assert!(app.state().chat_input.is_empty());
    }

    #[test]
    fn test_plan_command() {
        let mut app = App::new();
        app.state_mut().chat_input = "/plan Build a REST API".to_string();

        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(app.state().pending_action, Some(PendingAction::CreatePlan(_))));
    }

    #[test]
    fn test_clear_command() {
        let mut app = App::new();
        app.state_mut().chat_history.push(ChatMessage::user("test"));
        app.state_mut().chat_input = "/clear".to_string();

        let history_len_before = app.state().chat_history.len();
        app.handle_key(key(KeyCode::Enter));

        // History cleared and system message added
        assert_eq!(app.state().chat_history.len(), 1);
        assert!(history_len_before > 1);
    }

    #[test]
    fn test_loops_navigation() {
        let mut app = App::new();
        app.state_mut().interaction_mode = InteractionMode::Normal;
        app.state_mut().current_view = View::Loops;

        // Create some test loops
        use crate::store::LoopRecord;
        let records = vec![LoopRecord::new_plan("Task 1", 10), LoopRecord::new_plan("Task 2", 10)];
        app.state_mut().loops_tree.build_from_records(records);

        // Navigate
        app.handle_key(key(KeyCode::Char('j')));
        // Selection should have moved
    }

    #[test]
    fn test_ctrl_c_quits() {
        let mut app = App::new();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        let should_quit = app.handle_key(key);
        assert!(should_quit);
    }
}
