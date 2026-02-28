use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, InputMode, IpcAction};

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
    None,
}

/// Map a key event to an Action based on current input mode.
pub fn handle_key(key: KeyEvent, mode: InputMode) -> Action {
    match mode {
        InputMode::GoalInput => match key.code {
            KeyCode::Enter => Action::GoalSubmit,
            KeyCode::Esc => Action::GoalCancel,
            KeyCode::Backspace => Action::GoalBackspace,
            KeyCode::Char(c) => Action::GoalChar(c),
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
        Action::None => {}
    }
}

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
        assert_eq!(app.current_view, crate::tui::app::View::WorkItems);
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
            crate::agents::AgentSession::new(crate::agents::AgentType::Coordinator, "test-model".to_string());
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
            crate::agents::AgentSession::new(crate::agents::AgentType::Coordinator, "test-model".to_string());
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
            crate::agents::AgentSession::new(crate::agents::AgentType::Coordinator, "test-model".to_string());
        session.status = crate::agents::AgentStatus::Running;
        let session_id = session.id.clone();
        app.state.agent_sessions.push(session);
        apply_action(&mut app, Action::StopCoordinator);
        assert_eq!(app.pending_ipc, Some(IpcAction::StopAgent(session_id)));
    }
}
