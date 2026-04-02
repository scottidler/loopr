use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, ChatMessage, ChatMode, FunnelState, InputMode, IpcAction};

/// Action resulting from a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    NextView,
    PrevView,
    SelectNext,
    SelectPrev,
    CycleRole,
    ToggleHelp,
    Quit,
    /// Enter goal-input mode.
    EnterGoalInput,
    /// Append a character to the goal input buffer.
    GoalChar(char),
    /// Submit the goal input.
    GoalSubmit,
    /// Cancel goal input.
    GoalCancel,
    /// Delete last character from goal input.
    GoalBackspace,
    /// Pause the running Coordinator.
    PauseCoordinator,
    /// Resume the paused Coordinator.
    ResumeCoordinator,
    /// Stop the Coordinator.
    StopCoordinator,
    /// Create a new record (context-dependent on current view).
    NewRecord,
    /// Transition selected record's status.
    TransitionRecord,
    // Chat input actions
    /// Insert a character at cursor position.
    ChatChar(char),
    /// Submit the chat input.
    ChatSubmit,
    /// Delete character before cursor (Backspace).
    ChatBackspace,
    /// Delete character after cursor (Delete).
    ChatDelete,
    /// Move cursor left.
    ChatCursorLeft,
    /// Move cursor right.
    ChatCursorRight,
    /// Move cursor to start of input.
    ChatCursorHome,
    /// Move cursor to end of input.
    ChatCursorEnd,
    /// Enter scroll mode (Esc in chat).
    ChatEnterScroll,
    /// Scroll history up.
    ChatScrollUp,
    /// Scroll history down.
    ChatScrollDown,
    /// Scroll to top.
    ChatScrollTop,
    /// Scroll to bottom (auto-scroll).
    ChatScrollBottom,
    /// Page up in chat history.
    ChatPageUp,
    /// Page down in chat history.
    ChatPageDown,
    /// Approve plan (Ctrl+a in Plan mode).
    AcceptPlan,
    None,
}

/// UTF-8 safe: find previous char boundary before `pos`.
pub fn prev_char_boundary(input: &str, pos: usize) -> usize {
    let mut new_pos = pos.saturating_sub(1);
    while new_pos > 0 && !input.is_char_boundary(new_pos) {
        new_pos -= 1;
    }
    new_pos
}

/// UTF-8 safe: find next char boundary after `pos`.
pub fn next_char_boundary(input: &str, pos: usize) -> usize {
    let mut new_pos = pos + 1;
    while new_pos < input.len() && !input.is_char_boundary(new_pos) {
        new_pos += 1;
    }
    new_pos.min(input.len())
}

/// Map a key event to an Action based on current input mode.
pub fn handle_key(key: KeyEvent, mode: InputMode) -> Action {
    // Ctrl+c always quits
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }

    match mode {
        InputMode::GoalInput => match key.code {
            KeyCode::Enter => Action::GoalSubmit,
            KeyCode::Esc => Action::GoalCancel,
            KeyCode::Backspace => Action::GoalBackspace,
            KeyCode::Char(c) => Action::GoalChar(c),
            _ => Action::None,
        },
        InputMode::ChatInput => {
            // Ctrl+a to approve plan
            if key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Action::AcceptPlan;
            }
            match key.code {
                KeyCode::Enter => {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        Action::ChatChar('\n')
                    } else {
                        Action::ChatSubmit
                    }
                }
                KeyCode::Esc => Action::ChatEnterScroll,
                KeyCode::Backspace => Action::ChatBackspace,
                KeyCode::Delete => Action::ChatDelete,
                KeyCode::Left => Action::ChatCursorLeft,
                KeyCode::Right => Action::ChatCursorRight,
                KeyCode::Home => Action::ChatCursorHome,
                KeyCode::End => Action::ChatCursorEnd,
                KeyCode::PageUp => Action::ChatPageUp,
                KeyCode::PageDown => Action::ChatPageDown,
                KeyCode::Tab => {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        Action::PrevView
                    } else {
                        Action::NextView
                    }
                }
                KeyCode::BackTab => Action::PrevView,
                KeyCode::Char(c) => Action::ChatChar(c),
                _ => Action::None,
            }
        }
        InputMode::ChatScroll => match key.code {
            KeyCode::Esc => Action::None, // stay in scroll mode
            KeyCode::Char('j') | KeyCode::Down => Action::ChatScrollDown,
            KeyCode::Char('k') | KeyCode::Up => Action::ChatScrollUp,
            KeyCode::Char('g') => Action::ChatScrollTop,
            KeyCode::Char('G') => Action::ChatScrollBottom,
            KeyCode::PageUp => Action::ChatPageUp,
            KeyCode::PageDown => Action::ChatPageDown,
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    Action::PrevView
                } else {
                    Action::NextView
                }
            }
            KeyCode::BackTab => Action::PrevView,
            // Any other printable char exits scroll mode and inserts
            KeyCode::Char(c) => Action::ChatChar(c),
            _ => Action::None,
        },
        InputMode::Normal => match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Char('?') => Action::ToggleHelp,
            KeyCode::Char('R') => Action::CycleRole,
            KeyCode::Char('g') => Action::EnterGoalInput,
            KeyCode::Char('p') => Action::PauseCoordinator,
            KeyCode::Char('r') => Action::ResumeCoordinator,
            KeyCode::Char('x') => Action::StopCoordinator,
            KeyCode::Char('n') => Action::NewRecord,
            KeyCode::Char('t') => Action::TransitionRecord,
            KeyCode::Char('j') | KeyCode::Down => Action::SelectNext,
            KeyCode::Char('k') | KeyCode::Up => Action::SelectPrev,
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    Action::PrevView
                } else {
                    Action::NextView
                }
            }
            KeyCode::BackTab => Action::PrevView,
            _ => Action::None,
        },
    }
}

/// Parse a slash command from input. Returns Some(command) if input starts with '/'.
fn parse_slash_command(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if trimmed.starts_with('/') { Some(trimmed) } else { None }
}

/// Apply an action to the app state.
pub fn apply_action(app: &mut App, action: Action) {
    match action {
        Action::NextView => app.next_view(),
        Action::PrevView => app.prev_view(),
        Action::SelectNext => app.select_next(),
        Action::SelectPrev => app.select_prev(),
        Action::CycleRole => app.cycle_role(),
        Action::ToggleHelp => app.toggle_help(),
        Action::Quit => app.should_quit = true,
        Action::EnterGoalInput => {
            app.input_mode = InputMode::GoalInput;
            app.goal_input.clear();
        }
        Action::GoalChar(c) => {
            app.goal_input.push(c);
        }
        Action::GoalSubmit => {
            if !app.goal_input.is_empty() {
                app.pending_ipc = Some(IpcAction::SetGoal(app.goal_input.clone()));
            }
            app.goal_input.clear();
            app.input_mode = InputMode::Normal;
        }
        Action::GoalCancel => {
            app.goal_input.clear();
            app.input_mode = InputMode::Normal;
        }
        Action::GoalBackspace => {
            app.goal_input.pop();
        }
        Action::PauseCoordinator => {
            if let Some(session_id) = app.find_coordinator_session(true) {
                app.pending_ipc = Some(IpcAction::PauseAgent(session_id));
            }
        }
        Action::ResumeCoordinator => {
            if let Some(session_id) = app.find_coordinator_session(false) {
                app.pending_ipc = Some(IpcAction::ResumeAgent(session_id));
            }
        }
        Action::StopCoordinator => {
            // Try running first, then paused
            if let Some(session_id) = app
                .find_coordinator_session(true)
                .or_else(|| app.find_coordinator_session(false))
            {
                app.pending_ipc = Some(IpcAction::StopAgent(session_id));
            }
        }
        Action::NewRecord => {
            if let Some(collection) = app.view_collection() {
                app.pending_ipc = Some(IpcAction::NewRecord { collection });
            }
        }
        Action::TransitionRecord => {
            if let Some(collection) = app.view_collection()
                && let Some(id) = app.selected_record_id()
            {
                app.pending_ipc = Some(IpcAction::TransitionRecord { collection, id });
            }
        }
        // Chat input actions
        Action::ChatChar(c) => {
            // If in scroll mode, exit to input mode first
            if app.input_mode == InputMode::ChatScroll {
                app.input_mode = InputMode::ChatInput;
                app.chat_scroll = None;
            }
            app.chat_input.insert(app.chat_cursor_pos, c);
            app.chat_cursor_pos += c.len_utf8();
        }
        Action::ChatSubmit => {
            if app.chat_streaming {
                return; // don't submit while streaming
            }
            let input = app.chat_input.trim().to_string();
            if input.is_empty() {
                return;
            }
            app.chat_input.clear();
            app.chat_cursor_pos = 0;
            app.chat_scroll = None;

            // Check for slash commands
            if let Some(cmd) = parse_slash_command(&input) {
                match cmd {
                    "/plan" if app.funnel_state == FunnelState::Chat => {
                        app.chat_mode = ChatMode::Plan;
                        app.funnel_state = FunnelState::Interview;
                        app.chat_history
                            .push(ChatMessage::system("Entering Plan mode. Focusing on your goal.".into()));
                    }
                    "/plan" => {
                        // Already in Plan mode — ignore
                    }
                    "/chat" if matches!(app.funnel_state, FunnelState::Interview | FunnelState::PlanDraft) => {
                        app.chat_mode = ChatMode::Chat;
                        app.funnel_state = FunnelState::Chat;
                        app.chat_history
                            .push(ChatMessage::system("Returned to Chat mode.".into()));
                    }
                    "/chat" => {
                        // Already in Chat — ignore
                    }
                    "/clear" if app.funnel_state == FunnelState::Chat => {
                        app.chat_history.clear();
                        app.canonical_messages.clear();
                        app.chat_response_buffer.clear();
                        app.chat_streaming = false;
                        app.chat_mode = ChatMode::Chat;
                        app.funnel_state = FunnelState::Chat;
                    }
                    "/clear" => {
                        app.chat_history.push(ChatMessage::system(
                            "Use /chat first to return to Chat mode before clearing.".into(),
                        ));
                    }
                    "/draft" if app.funnel_state == FunnelState::Interview => {
                        app.funnel_state = FunnelState::PlanDraft;
                        app.chat_history
                            .push(ChatMessage::system("Building draft plan...".into()));
                        app.pending_chat_submit = Some("/draft".into());
                    }
                    "/draft" => {
                        app.chat_history
                            .push(ChatMessage::system("Use /plan first to enter Plan mode.".into()));
                    }
                    "/accept" if app.funnel_state == FunnelState::PlanDraft => {
                        // Extract plan text and hand off to orchestration
                        let plan_text = app
                            .chat_history
                            .iter()
                            .rev()
                            .find(|m| m.role == super::app::ChatRole::Assistant)
                            .map(|m| m.content.clone())
                            .unwrap_or_default();
                        if !plan_text.is_empty() {
                            app.pending_ipc = Some(IpcAction::AcceptPlan(plan_text));
                            app.funnel_state = FunnelState::Executing;
                            // Stay on Chat view - orchestration events will stream here
                            app.chat_history
                                .push(ChatMessage::system("Plan accepted! Starting orchestration.".into()));
                        }
                    }
                    "/accept" => {
                        app.chat_history.push(ChatMessage::system(
                            "Use /draft first to generate a plan before accepting.".into(),
                        ));
                    }
                    "/pause" if app.funnel_state == FunnelState::Executing => {
                        app.pending_ipc = Some(IpcAction::PauseAgent("coordinator".to_string()));
                        app.chat_history
                            .push(ChatMessage::system("Pausing Coordinator...".into()));
                    }
                    "/stop" if app.funnel_state == FunnelState::Executing => {
                        app.pending_ipc = Some(IpcAction::StopAgent("coordinator".to_string()));
                        app.chat_history
                            .push(ChatMessage::system("Stopping orchestration...".into()));
                    }
                    "/status" if app.funnel_state == FunnelState::Executing => {
                        // Status is shown via orchestration events in chat + system prompt
                        app.chat_history
                            .push(ChatMessage::system("Status is shown in the orchestration events above. Send a message to ask the assistant for a summary.".into()));
                    }
                    "/help" => {
                        let help = match app.funnel_state {
                            FunnelState::Chat => {
                                "Commands: /plan (enter Plan mode), /clear (clear history), /help (this)"
                            }
                            FunnelState::Interview => {
                                "Commands: /draft (build draft), /chat (back to Chat), /help (this)"
                            }
                            FunnelState::PlanDraft => {
                                "Commands: /accept or Ctrl+a (accept plan), /chat (back to Chat), /help (this)"
                            }
                            FunnelState::Executing => "Orchestration running. Commands: /status, /pause, /stop, /help",
                        };
                        app.chat_history.push(ChatMessage::system(help.into()));
                    }
                    _ => {
                        app.chat_history
                            .push(ChatMessage::system(format!("Unknown command: {cmd}")));
                    }
                }
            } else {
                // Regular message — always goes to TUI-side LLM
                app.chat_history.push(ChatMessage::user(input.clone()));
                // Also append to canonical messages for the Anthropic API
                app.canonical_messages.push(crate::tools::types::Message {
                    role: "user".to_string(),
                    content: vec![crate::tools::types::ContentBlock::Text { text: input.clone() }],
                });
                app.pending_chat_submit = Some(input);
            }
        }
        Action::ChatBackspace => {
            if app.chat_cursor_pos > 0 {
                let new_pos = prev_char_boundary(&app.chat_input, app.chat_cursor_pos);
                app.chat_input.drain(new_pos..app.chat_cursor_pos);
                app.chat_cursor_pos = new_pos;
            }
        }
        Action::ChatDelete => {
            if app.chat_cursor_pos < app.chat_input.len() {
                let end = next_char_boundary(&app.chat_input, app.chat_cursor_pos);
                app.chat_input.drain(app.chat_cursor_pos..end);
            }
        }
        Action::ChatCursorLeft => {
            if app.chat_cursor_pos > 0 {
                app.chat_cursor_pos = prev_char_boundary(&app.chat_input, app.chat_cursor_pos);
            }
        }
        Action::ChatCursorRight => {
            if app.chat_cursor_pos < app.chat_input.len() {
                app.chat_cursor_pos = next_char_boundary(&app.chat_input, app.chat_cursor_pos);
            }
        }
        Action::ChatCursorHome => {
            app.chat_cursor_pos = 0;
        }
        Action::ChatCursorEnd => {
            app.chat_cursor_pos = app.chat_input.len();
        }
        Action::ChatEnterScroll => {
            app.input_mode = InputMode::ChatScroll;
        }
        Action::ChatScrollUp => {
            let scroll = app.chat_scroll.unwrap_or(0);
            app.chat_scroll = Some(scroll.saturating_add(1));
        }
        Action::ChatScrollDown => {
            if let Some(scroll) = app.chat_scroll {
                if scroll <= 1 {
                    app.chat_scroll = None; // back to auto-scroll
                } else {
                    app.chat_scroll = Some(scroll - 1);
                }
            }
        }
        Action::ChatScrollTop => {
            // Large number, render will clamp
            app.chat_scroll = Some(usize::MAX);
        }
        Action::ChatScrollBottom => {
            app.chat_scroll = None; // auto-scroll
            app.input_mode = InputMode::ChatInput;
        }
        Action::ChatPageUp => {
            let scroll = app.chat_scroll.unwrap_or(0);
            app.chat_scroll = Some(scroll.saturating_add(10));
        }
        Action::ChatPageDown => {
            if let Some(scroll) = app.chat_scroll {
                app.chat_scroll = Some(scroll.saturating_sub(10));
                if app.chat_scroll == Some(0) {
                    app.chat_scroll = None;
                }
            }
        }
        Action::AcceptPlan => {
            if app.funnel_state == FunnelState::PlanDraft {
                // Extract plan text from the last assistant message
                let plan_text = app
                    .chat_history
                    .iter()
                    .rev()
                    .find(|m| m.role == super::app::ChatRole::Assistant)
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                if !plan_text.is_empty() {
                    app.pending_ipc = Some(IpcAction::AcceptPlan(plan_text));
                    app.funnel_state = FunnelState::Executing;
                    // Stay on Chat view - orchestration events will stream here
                    app.chat_history
                        .push(ChatMessage::system("Plan accepted! Starting orchestration.".into()));
                }
            }
        }
        Action::None => {}
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn key_with_mods(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    // --- Normal mode tests (unchanged) ---

    #[test]
    fn test_quit_key() {
        assert_eq!(handle_key(key(KeyCode::Char('q')), InputMode::Normal), Action::Quit);
    }

    #[test]
    fn test_help_key() {
        assert_eq!(
            handle_key(key(KeyCode::Char('?')), InputMode::Normal),
            Action::ToggleHelp
        );
    }

    #[test]
    fn test_role_key_shifted() {
        assert_eq!(
            handle_key(key(KeyCode::Char('R')), InputMode::Normal),
            Action::CycleRole
        );
    }

    #[test]
    fn test_navigation_keys() {
        assert_eq!(
            handle_key(key(KeyCode::Char('j')), InputMode::Normal),
            Action::SelectNext
        );
        assert_eq!(handle_key(key(KeyCode::Down), InputMode::Normal), Action::SelectNext);
        assert_eq!(
            handle_key(key(KeyCode::Char('k')), InputMode::Normal),
            Action::SelectPrev
        );
        assert_eq!(handle_key(key(KeyCode::Up), InputMode::Normal), Action::SelectPrev);
    }

    #[test]
    fn test_tab_keys() {
        assert_eq!(handle_key(key(KeyCode::Tab), InputMode::Normal), Action::NextView);
        assert_eq!(
            handle_key(key_with_mods(KeyCode::Tab, KeyModifiers::SHIFT), InputMode::Normal),
            Action::PrevView
        );
        assert_eq!(handle_key(key(KeyCode::BackTab), InputMode::Normal), Action::PrevView);
    }

    #[test]
    fn test_coordinator_keys() {
        assert_eq!(
            handle_key(key(KeyCode::Char('g')), InputMode::Normal),
            Action::EnterGoalInput
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('p')), InputMode::Normal),
            Action::PauseCoordinator
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('r')), InputMode::Normal),
            Action::ResumeCoordinator
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('x')), InputMode::Normal),
            Action::StopCoordinator
        );
    }

    #[test]
    fn test_new_record_key() {
        assert_eq!(
            handle_key(key(KeyCode::Char('n')), InputMode::Normal),
            Action::NewRecord
        );
    }

    #[test]
    fn test_transition_record_key() {
        assert_eq!(
            handle_key(key(KeyCode::Char('t')), InputMode::Normal),
            Action::TransitionRecord
        );
    }

    #[test]
    fn test_unknown_key() {
        assert_eq!(handle_key(key(KeyCode::Char('z')), InputMode::Normal), Action::None);
    }

    #[test]
    fn test_goal_input_mode() {
        assert_eq!(
            handle_key(key(KeyCode::Char('a')), InputMode::GoalInput),
            Action::GoalChar('a')
        );
        assert_eq!(
            handle_key(key(KeyCode::Enter), InputMode::GoalInput),
            Action::GoalSubmit
        );
        assert_eq!(handle_key(key(KeyCode::Esc), InputMode::GoalInput), Action::GoalCancel);
        assert_eq!(
            handle_key(key(KeyCode::Backspace), InputMode::GoalInput),
            Action::GoalBackspace
        );
        // Other keys in goal mode are no-ops
        assert_eq!(handle_key(key(KeyCode::Tab), InputMode::GoalInput), Action::None);
    }

    // --- Chat input mode tests ---

    #[test]
    fn test_chat_input_char() {
        assert_eq!(
            handle_key(key(KeyCode::Char('a')), InputMode::ChatInput),
            Action::ChatChar('a')
        );
    }

    #[test]
    fn test_chat_input_submit() {
        assert_eq!(
            handle_key(key(KeyCode::Enter), InputMode::ChatInput),
            Action::ChatSubmit
        );
    }

    #[test]
    fn test_chat_input_backspace() {
        assert_eq!(
            handle_key(key(KeyCode::Backspace), InputMode::ChatInput),
            Action::ChatBackspace
        );
    }

    #[test]
    fn test_chat_input_cursor_movement() {
        assert_eq!(
            handle_key(key(KeyCode::Left), InputMode::ChatInput),
            Action::ChatCursorLeft
        );
        assert_eq!(
            handle_key(key(KeyCode::Right), InputMode::ChatInput),
            Action::ChatCursorRight
        );
        assert_eq!(
            handle_key(key(KeyCode::Home), InputMode::ChatInput),
            Action::ChatCursorHome
        );
        assert_eq!(
            handle_key(key(KeyCode::End), InputMode::ChatInput),
            Action::ChatCursorEnd
        );
    }

    #[test]
    fn test_chat_input_esc_enters_scroll() {
        assert_eq!(
            handle_key(key(KeyCode::Esc), InputMode::ChatInput),
            Action::ChatEnterScroll
        );
    }

    #[test]
    fn test_chat_input_tab_switches_view() {
        assert_eq!(handle_key(key(KeyCode::Tab), InputMode::ChatInput), Action::NextView);
        assert_eq!(
            handle_key(key_with_mods(KeyCode::Tab, KeyModifiers::SHIFT), InputMode::ChatInput),
            Action::PrevView
        );
    }

    #[test]
    fn test_chat_input_ctrl_c_quits() {
        assert_eq!(
            handle_key(
                key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL),
                InputMode::ChatInput
            ),
            Action::Quit
        );
    }

    #[test]
    fn test_chat_input_ctrl_a_approves_plan() {
        assert_eq!(
            handle_key(
                key_with_mods(KeyCode::Char('a'), KeyModifiers::CONTROL),
                InputMode::ChatInput
            ),
            Action::AcceptPlan
        );
    }

    // --- Chat scroll mode tests ---

    #[test]
    fn test_chat_scroll_jk() {
        assert_eq!(
            handle_key(key(KeyCode::Char('j')), InputMode::ChatScroll),
            Action::ChatScrollDown
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('k')), InputMode::ChatScroll),
            Action::ChatScrollUp
        );
    }

    #[test]
    fn test_chat_scroll_g_and_big_g() {
        assert_eq!(
            handle_key(key(KeyCode::Char('g')), InputMode::ChatScroll),
            Action::ChatScrollTop
        );
        assert_eq!(
            handle_key(key(KeyCode::Char('G')), InputMode::ChatScroll),
            Action::ChatScrollBottom
        );
    }

    #[test]
    fn test_chat_scroll_esc_stays() {
        assert_eq!(handle_key(key(KeyCode::Esc), InputMode::ChatScroll), Action::None);
    }

    #[test]
    fn test_chat_scroll_printable_exits_to_input() {
        // Any non-navigation printable char should insert (exits scroll mode in apply_action)
        assert_eq!(
            handle_key(key(KeyCode::Char('a')), InputMode::ChatScroll),
            Action::ChatChar('a')
        );
    }

    // --- Apply action tests ---

    #[test]
    fn test_apply_quit() {
        let mut app = App::new();
        assert!(!app.should_quit);
        apply_action(&mut app, Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn test_apply_next_view() {
        let mut app = App::new();
        apply_action(&mut app, Action::NextView);
        assert_eq!(app.current_view, crate::tui::app::View::Dashboard);
    }

    #[test]
    fn test_apply_none_is_noop() {
        let mut app = App::new();
        let view_before = app.current_view;
        let role_before = app.current_role;
        apply_action(&mut app, Action::None);
        assert_eq!(app.current_view, view_before);
        assert_eq!(app.current_role, role_before);
    }

    #[test]
    fn test_apply_enter_goal_input() {
        let mut app = App::new();
        apply_action(&mut app, Action::EnterGoalInput);
        assert_eq!(app.input_mode, InputMode::GoalInput);
        assert!(app.goal_input.is_empty());
    }

    #[test]
    fn test_apply_goal_char_and_submit() {
        let mut app = App::new();
        app.input_mode = InputMode::GoalInput;
        apply_action(&mut app, Action::GoalChar('h'));
        apply_action(&mut app, Action::GoalChar('i'));
        assert_eq!(app.goal_input, "hi");
        apply_action(&mut app, Action::GoalSubmit);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.goal_input.is_empty());
        assert_eq!(app.pending_ipc, Some(IpcAction::SetGoal("hi".to_string())));
    }

    #[test]
    fn test_apply_goal_cancel() {
        let mut app = App::new();
        app.input_mode = InputMode::GoalInput;
        apply_action(&mut app, Action::GoalChar('x'));
        apply_action(&mut app, Action::GoalCancel);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.goal_input.is_empty());
        assert!(app.pending_ipc.is_none());
    }

    #[test]
    fn test_apply_goal_backspace() {
        let mut app = App::new();
        app.input_mode = InputMode::GoalInput;
        apply_action(&mut app, Action::GoalChar('a'));
        apply_action(&mut app, Action::GoalChar('b'));
        apply_action(&mut app, Action::GoalBackspace);
        assert_eq!(app.goal_input, "a");
    }

    #[test]
    fn test_apply_goal_submit_empty_no_ipc() {
        let mut app = App::new();
        app.input_mode = InputMode::GoalInput;
        apply_action(&mut app, Action::GoalSubmit);
        assert!(app.pending_ipc.is_none());
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn test_apply_pause_no_coordinator() {
        let mut app = App::new();
        apply_action(&mut app, Action::PauseCoordinator);
        assert!(app.pending_ipc.is_none());
    }

    #[test]
    fn test_apply_pause_with_running_coordinator() {
        let mut app = App::new();
        let mut session =
            crate::agents::AgentSession::new(crate::agents::AgentKind::Coordinator, "test-model".to_string());
        session.status = crate::agents::AgentStatus::Running;
        let session_id = session.id.clone();
        app.state.agent_sessions.push(session);
        apply_action(&mut app, Action::PauseCoordinator);
        assert_eq!(app.pending_ipc, Some(IpcAction::PauseAgent(session_id)));
    }

    #[test]
    fn test_apply_resume_with_paused_coordinator() {
        let mut app = App::new();
        let mut session =
            crate::agents::AgentSession::new(crate::agents::AgentKind::Coordinator, "test-model".to_string());
        session.status = crate::agents::AgentStatus::Paused;
        let session_id = session.id.clone();
        app.state.agent_sessions.push(session);
        apply_action(&mut app, Action::ResumeCoordinator);
        assert_eq!(app.pending_ipc, Some(IpcAction::ResumeAgent(session_id)));
    }

    #[test]
    fn test_apply_stop_coordinator() {
        let mut app = App::new();
        let mut session =
            crate::agents::AgentSession::new(crate::agents::AgentKind::Coordinator, "test-model".to_string());
        session.status = crate::agents::AgentStatus::Running;
        let session_id = session.id.clone();
        app.state.agent_sessions.push(session);
        apply_action(&mut app, Action::StopCoordinator);
        assert_eq!(app.pending_ipc, Some(IpcAction::StopAgent(session_id)));
    }

    // --- Chat action tests ---

    #[test]
    fn test_apply_chat_char() {
        let mut app = App::new();
        apply_action(&mut app, Action::ChatChar('h'));
        apply_action(&mut app, Action::ChatChar('i'));
        assert_eq!(app.chat_input, "hi");
        assert_eq!(app.chat_cursor_pos, 2);
    }

    #[test]
    fn test_apply_chat_submit() {
        let mut app = App::new();
        apply_action(&mut app, Action::ChatChar('h'));
        apply_action(&mut app, Action::ChatChar('i'));
        apply_action(&mut app, Action::ChatSubmit);
        assert!(app.chat_input.is_empty());
        assert_eq!(app.chat_cursor_pos, 0);
        assert_eq!(app.chat_history.len(), 1);
        assert_eq!(app.chat_history[0].content, "hi");
        assert_eq!(app.chat_history[0].role, super::super::app::ChatRole::User);
        assert_eq!(app.pending_chat_submit, Some("hi".to_string()));
    }

    #[test]
    fn test_apply_chat_submit_empty_is_noop() {
        let mut app = App::new();
        apply_action(&mut app, Action::ChatSubmit);
        assert!(app.chat_history.is_empty());
        assert!(app.pending_chat_submit.is_none());
    }

    #[test]
    fn test_apply_chat_submit_while_streaming_is_noop() {
        let mut app = App::new();
        app.chat_streaming = true;
        apply_action(&mut app, Action::ChatChar('x'));
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.chat_input, "x"); // not cleared
        assert!(app.chat_history.is_empty());
    }

    #[test]
    fn test_apply_chat_backspace() {
        let mut app = App::new();
        apply_action(&mut app, Action::ChatChar('a'));
        apply_action(&mut app, Action::ChatChar('b'));
        apply_action(&mut app, Action::ChatBackspace);
        assert_eq!(app.chat_input, "a");
        assert_eq!(app.chat_cursor_pos, 1);
    }

    #[test]
    fn test_apply_chat_backspace_at_start() {
        let mut app = App::new();
        apply_action(&mut app, Action::ChatBackspace);
        assert!(app.chat_input.is_empty());
        assert_eq!(app.chat_cursor_pos, 0);
    }

    #[test]
    fn test_apply_chat_cursor_movement() {
        let mut app = App::new();
        apply_action(&mut app, Action::ChatChar('a'));
        apply_action(&mut app, Action::ChatChar('b'));
        apply_action(&mut app, Action::ChatChar('c'));
        assert_eq!(app.chat_cursor_pos, 3);

        apply_action(&mut app, Action::ChatCursorLeft);
        assert_eq!(app.chat_cursor_pos, 2);

        apply_action(&mut app, Action::ChatCursorHome);
        assert_eq!(app.chat_cursor_pos, 0);

        apply_action(&mut app, Action::ChatCursorEnd);
        assert_eq!(app.chat_cursor_pos, 3);

        apply_action(&mut app, Action::ChatCursorRight);
        assert_eq!(app.chat_cursor_pos, 3); // already at end
    }

    #[test]
    fn test_apply_chat_enter_scroll() {
        let mut app = App::new();
        apply_action(&mut app, Action::ChatEnterScroll);
        assert_eq!(app.input_mode, InputMode::ChatScroll);
    }

    #[test]
    fn test_apply_chat_scroll_up_down() {
        let mut app = App::new();
        apply_action(&mut app, Action::ChatScrollUp);
        assert_eq!(app.chat_scroll, Some(1));
        apply_action(&mut app, Action::ChatScrollUp);
        assert_eq!(app.chat_scroll, Some(2));
        apply_action(&mut app, Action::ChatScrollDown);
        assert_eq!(app.chat_scroll, Some(1));
        apply_action(&mut app, Action::ChatScrollDown);
        assert_eq!(app.chat_scroll, None); // back to auto-scroll
    }

    #[test]
    fn test_apply_chat_scroll_bottom_returns_to_input() {
        let mut app = App::new();
        app.input_mode = InputMode::ChatScroll;
        app.chat_scroll = Some(10);
        apply_action(&mut app, Action::ChatScrollBottom);
        assert_eq!(app.chat_scroll, None);
        assert_eq!(app.input_mode, InputMode::ChatInput);
    }

    #[test]
    fn test_chat_char_exits_scroll_mode() {
        let mut app = App::new();
        app.input_mode = InputMode::ChatScroll;
        app.chat_scroll = Some(5);
        apply_action(&mut app, Action::ChatChar('a'));
        assert_eq!(app.input_mode, InputMode::ChatInput);
        assert_eq!(app.chat_scroll, None);
        assert_eq!(app.chat_input, "a");
    }

    #[test]
    fn test_slash_command_plan() {
        let mut app = App::new();
        app.chat_input = "/plan".to_string();
        app.chat_cursor_pos = 5;
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.chat_mode, ChatMode::Plan);
        assert_eq!(app.funnel_state, FunnelState::Interview);
        assert!(app.pending_ipc.is_none()); // no IPC — local state only
        assert!(
            app.chat_history
                .iter()
                .any(|m| m.role == super::super::app::ChatRole::System)
        );
    }

    #[test]
    fn test_slash_command_chat() {
        let mut app = App::new();
        app.chat_mode = ChatMode::Plan;
        app.funnel_state = FunnelState::Interview;
        app.chat_input = "/chat".to_string();
        app.chat_cursor_pos = 5;
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.chat_mode, ChatMode::Chat);
        assert_eq!(app.funnel_state, FunnelState::Chat);
    }

    #[test]
    fn test_slash_command_clear() {
        let mut app = App::new();
        app.chat_history.push(ChatMessage::user("hello".into()));
        app.chat_input = "/clear".to_string();
        app.chat_cursor_pos = 6;
        apply_action(&mut app, Action::ChatSubmit);
        assert!(app.chat_history.is_empty());
    }

    #[test]
    fn test_slash_command_help() {
        let mut app = App::new();
        app.chat_input = "/help".to_string();
        app.chat_cursor_pos = 5;
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.chat_history.len(), 1);
        assert!(app.chat_history[0].content.contains("/plan"));
    }

    // --- UTF-8 cursor helper tests ---

    #[test]
    fn test_prev_char_boundary_ascii() {
        assert_eq!(prev_char_boundary("hello", 3), 2);
        assert_eq!(prev_char_boundary("hello", 1), 0);
        assert_eq!(prev_char_boundary("hello", 0), 0);
    }

    #[test]
    fn test_next_char_boundary_ascii() {
        assert_eq!(next_char_boundary("hello", 0), 1);
        assert_eq!(next_char_boundary("hello", 4), 5);
    }

    #[test]
    fn test_char_boundary_multibyte() {
        let s = "héllo"; // é is 2 bytes
        assert_eq!(next_char_boundary(s, 0), 1); // h -> é
        assert_eq!(next_char_boundary(s, 1), 3); // é (2 bytes) -> l
        assert_eq!(prev_char_boundary(s, 3), 1); // l -> é
    }

    #[test]
    fn test_apply_approve_plan_in_plan_draft() {
        let mut app = App::new();
        app.funnel_state = FunnelState::PlanDraft;
        app.chat_history
            .push(ChatMessage::assistant("Title: My Plan\nGoal: Do stuff".into()));
        apply_action(&mut app, Action::AcceptPlan);
        assert_eq!(
            app.pending_ipc,
            Some(IpcAction::AcceptPlan("Title: My Plan\nGoal: Do stuff".into()))
        );
        assert_eq!(app.funnel_state, FunnelState::Executing);
        // Stays on Chat view (not Dashboard) - orchestration events stream here
        assert_eq!(app.current_view, crate::tui::app::View::Chat);
        assert!(app.chat_history.iter().any(|m| m.content.contains("orchestration")));
    }

    #[test]
    fn test_apply_approve_plan_not_in_plan_draft() {
        let mut app = App::new();
        apply_action(&mut app, Action::AcceptPlan);
        assert!(app.pending_ipc.is_none()); // not in PlanDraft state
    }

    #[test]
    fn test_apply_approve_plan_no_assistant_message() {
        let mut app = App::new();
        app.funnel_state = FunnelState::PlanDraft;
        // No assistant messages in history
        apply_action(&mut app, Action::AcceptPlan);
        assert!(app.pending_ipc.is_none()); // empty plan text, no IPC
    }

    #[test]
    fn test_shift_enter_inserts_newline() {
        assert_eq!(
            handle_key(key_with_mods(KeyCode::Enter, KeyModifiers::SHIFT), InputMode::ChatInput),
            Action::ChatChar('\n')
        );
    }

    #[test]
    fn test_slash_command_draft_in_interview_state() {
        let mut app = App::new();
        app.chat_mode = ChatMode::Plan;
        app.funnel_state = FunnelState::Interview;
        app.chat_input = "/draft".to_string();
        app.chat_cursor_pos = 6;
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.funnel_state, FunnelState::PlanDraft);
        assert_eq!(app.pending_chat_submit, Some("/draft".into()));
    }

    #[test]
    fn test_slash_command_draft_in_chat_mode() {
        let mut app = App::new();
        app.chat_input = "/draft".to_string();
        app.chat_cursor_pos = 6;
        apply_action(&mut app, Action::ChatSubmit);
        assert!(app.pending_chat_submit.is_none());
        assert!(app.chat_history.iter().any(|m| m.content.contains("/plan first")));
    }

    #[test]
    fn test_funnel_state_transitions() {
        let mut app = App::new();
        assert_eq!(app.funnel_state, FunnelState::Chat);

        // /plan transitions to Interview
        app.chat_input = "/plan".to_string();
        app.chat_cursor_pos = 5;
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.funnel_state, FunnelState::Interview);

        // /chat transitions back to Chat
        app.chat_input = "/chat".to_string();
        app.chat_cursor_pos = 5;
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.funnel_state, FunnelState::Chat);

        // AcceptPlan transitions to Executing (requires PlanDraft state + assistant message)
        app.funnel_state = FunnelState::PlanDraft;
        app.chat_history.push(ChatMessage::assistant("The plan".into()));
        apply_action(&mut app, Action::AcceptPlan);
        assert_eq!(app.funnel_state, FunnelState::Executing);
    }

    #[test]
    fn test_slash_plan_ignored_when_already_in_interview() {
        let mut app = App::new();
        app.chat_mode = ChatMode::Plan;
        app.funnel_state = FunnelState::Interview;
        let history_len = app.chat_history.len();
        app.chat_input = "/plan".to_string();
        app.chat_cursor_pos = 5;
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.funnel_state, FunnelState::Interview);
        assert_eq!(app.chat_history.len(), history_len); // no message added
    }

    #[test]
    fn test_slash_accept_in_plan_draft() {
        let mut app = App::new();
        app.funnel_state = FunnelState::PlanDraft;
        app.chat_history.push(ChatMessage::assistant("Title: My Plan".into()));
        app.chat_input = "/accept".to_string();
        app.chat_cursor_pos = 7;
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.funnel_state, FunnelState::Executing);
        assert_eq!(app.pending_ipc, Some(IpcAction::AcceptPlan("Title: My Plan".into())));
    }

    #[test]
    fn test_slash_accept_not_in_plan_draft() {
        let mut app = App::new();
        app.chat_input = "/accept".to_string();
        app.chat_cursor_pos = 7;
        apply_action(&mut app, Action::ChatSubmit);
        assert_eq!(app.funnel_state, FunnelState::Chat);
        assert!(app.chat_history.iter().any(|m| m.content.contains("/draft first")));
    }

    #[test]
    fn test_slash_clear_only_in_chat() {
        let mut app = App::new();
        app.funnel_state = FunnelState::Interview;
        app.chat_history.push(ChatMessage::user("test".into()));
        app.chat_input = "/clear".to_string();
        app.chat_cursor_pos = 6;
        apply_action(&mut app, Action::ChatSubmit);
        // Should NOT clear — not in Chat state
        assert_eq!(app.chat_history.len(), 2); // original + error message
    }

    #[test]
    fn test_slash_help_context_sensitive() {
        let mut app = App::new();
        app.chat_input = "/help".to_string();
        app.chat_cursor_pos = 5;
        apply_action(&mut app, Action::ChatSubmit);
        assert!(app.chat_history[0].content.contains("/plan"));

        app.chat_history.clear();
        app.funnel_state = FunnelState::Interview;
        app.chat_input = "/help".to_string();
        app.chat_cursor_pos = 5;
        apply_action(&mut app, Action::ChatSubmit);
        assert!(app.chat_history[0].content.contains("/draft"));

        app.chat_history.clear();
        app.funnel_state = FunnelState::PlanDraft;
        app.chat_input = "/help".to_string();
        app.chat_cursor_pos = 5;
        apply_action(&mut app, Action::ChatSubmit);
        assert!(app.chat_history[0].content.contains("/accept"));
    }

    // --- Event replay tests (keystroke-to-state pipeline via type_and_submit) ---

    use crate::tui::app::ChatRole;
    use crate::tui::test_utils::{press_key, type_and_submit};

    #[test]
    fn test_replay_chat_submit_flow() {
        let mut app = App::new();

        type_and_submit(&mut app, "hello world");

        assert!(app.chat_input.is_empty());
        assert_eq!(app.chat_cursor_pos, 0);
        assert_eq!(app.chat_history.len(), 1);
        assert_eq!(app.chat_history[0].content, "hello world");
        assert_eq!(app.chat_history[0].role, ChatRole::User);
        assert_eq!(app.pending_chat_submit, Some("hello world".to_string()));
    }

    #[test]
    fn test_replay_plan_funnel_flow() {
        let mut app = App::new();

        // /plan transitions Chat -> Interview
        type_and_submit(&mut app, "/plan");
        assert_eq!(app.funnel_state, FunnelState::Interview);
        assert_eq!(app.chat_mode, ChatMode::Plan);
        assert!(app.chat_history.iter().any(|m| m.content.contains("Plan mode")));

        // Regular message in interview goes to LLM
        type_and_submit(&mut app, "Build a widget");
        assert_eq!(app.pending_chat_submit, Some("Build a widget".to_string()));

        // /draft transitions Interview -> PlanDraft
        type_and_submit(&mut app, "/draft");
        assert_eq!(app.funnel_state, FunnelState::PlanDraft);
        assert_eq!(app.pending_chat_submit, Some("/draft".to_string()));
    }

    #[test]
    fn test_replay_slash_wrong_state() {
        let mut app = App::new();

        // /draft without /plan first
        type_and_submit(&mut app, "/draft");
        assert_eq!(app.funnel_state, FunnelState::Chat);
        assert!(app.chat_history.iter().any(|m| m.content.contains("/plan first")));

        // /accept without /draft first
        type_and_submit(&mut app, "/accept");
        assert_eq!(app.funnel_state, FunnelState::Chat);
        assert!(app.chat_history.iter().any(|m| m.content.contains("/draft first")));
    }

    #[test]
    fn test_replay_clear_resets_state() {
        let mut app = App::new();

        type_and_submit(&mut app, "hello");
        assert!(!app.chat_history.is_empty());

        type_and_submit(&mut app, "/clear");
        assert!(app.chat_history.is_empty());
        assert!(app.canonical_messages.is_empty());
    }

    #[test]
    fn test_replay_unknown_command() {
        let mut app = App::new();

        type_and_submit(&mut app, "/nonexistent");
        assert!(app.chat_history.iter().any(|m| m.content.contains("Unknown command")));
    }

    #[test]
    fn test_replay_empty_submit_is_noop() {
        let mut app = App::new();

        // Just press Enter with no input
        press_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.chat_history.is_empty());
        assert!(app.pending_chat_submit.is_none());
    }

    #[test]
    fn test_replay_cursor_movement() {
        let mut app = App::new();

        // Type "abcde"
        for c in "abcde".chars() {
            press_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.chat_cursor_pos, 5);

        // Home moves to start
        press_key(&mut app, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(app.chat_cursor_pos, 0);

        // End moves to end
        press_key(&mut app, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(app.chat_cursor_pos, 5);

        // Left moves back one
        press_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.chat_cursor_pos, 4);

        // Right moves forward one
        press_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.chat_cursor_pos, 5);

        // Right at end stays put
        press_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.chat_cursor_pos, 5);
    }

    #[test]
    fn test_replay_cursor_utf8() {
        let mut app = App::new();

        // Type "café" - é is multi-byte (2 bytes in UTF-8)
        for c in "café".chars() {
            press_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.chat_input, "café");
        assert_eq!(app.chat_cursor_pos, app.chat_input.len()); // 5

        // Left moves back one char (past é) - cursor now before é
        press_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.chat_cursor_pos, 3); // before the 2-byte é
        assert!(app.chat_input.is_char_boundary(app.chat_cursor_pos));

        // Delete removes char at cursor (é), not before it
        press_key(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(app.chat_input, "caf");
    }

    #[test]
    fn test_replay_view_cycling_syncs_input_mode() {
        let mut app = App::new();
        assert_eq!(app.current_view, crate::tui::app::View::Chat);
        assert_eq!(app.input_mode, InputMode::ChatInput);

        // Tab switches to Dashboard (Normal mode)
        press_key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.current_view, crate::tui::app::View::Dashboard);
        assert_eq!(app.input_mode, InputMode::Normal);

        // BackTab returns to Chat (ChatInput mode)
        press_key(&mut app, KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(app.current_view, crate::tui::app::View::Chat);
        assert_eq!(app.input_mode, InputMode::ChatInput);
    }

    #[test]
    fn test_replay_scroll_mode_and_exit() {
        let mut app = App::new();

        // Esc enters scroll mode
        press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.input_mode, InputMode::ChatScroll);

        // j scrolls down
        press_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        // Any scroll action should set chat_scroll
        // (scroll down from None does nothing meaningful but doesn't crash)

        // Typing a printable char exits scroll mode and inserts
        press_key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(app.input_mode, InputMode::ChatInput);
        assert_eq!(app.chat_input, "x");
    }

    #[test]
    fn test_replay_backspace_on_empty_input() {
        let mut app = App::new();
        assert!(app.chat_input.is_empty());
        assert_eq!(app.chat_cursor_pos, 0);

        // Backspace on empty input should be a no-op
        press_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert!(app.chat_input.is_empty());
        assert_eq!(app.chat_cursor_pos, 0);
    }

    #[test]
    fn test_replay_delete_at_end() {
        let mut app = App::new();
        for c in "abc".chars() {
            press_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.chat_cursor_pos, 3);

        // Delete at end should be a no-op
        press_key(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(app.chat_input, "abc");
    }

    #[test]
    fn test_replay_delete_in_middle() {
        let mut app = App::new();
        for c in "abc".chars() {
            press_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        // Move cursor to position 1 (before 'b')
        press_key(&mut app, KeyCode::Home, KeyModifiers::NONE);
        press_key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.chat_cursor_pos, 1);

        // Delete should remove 'b'
        press_key(&mut app, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(app.chat_input, "ac");
        assert_eq!(app.chat_cursor_pos, 1);
    }

    #[test]
    fn test_replay_shift_enter_inserts_newline() {
        let mut app = App::new();
        for c in "line1".chars() {
            press_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }

        press_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);

        for c in "line2".chars() {
            press_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }

        assert_eq!(app.chat_input, "line1\nline2");
        // Not submitted - still in input
        assert!(app.chat_history.is_empty());
    }

    #[test]
    fn test_replay_ctrl_c_quits() {
        let mut app = App::new();
        assert!(!app.should_quit);

        press_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
    }
}
