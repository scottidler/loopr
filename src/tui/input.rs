use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::App;

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
    None,
}

/// Map a key event to an Action.
pub fn handle_key(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Char('r') => Action::CycleRole,
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
        assert_eq!(handle_key(key(KeyCode::Char('q'))), Action::Quit);
    }

    #[test]
    fn test_help_key() {
        assert_eq!(handle_key(key(KeyCode::Char('?'))), Action::ToggleHelp);
    }

    #[test]
    fn test_role_key() {
        assert_eq!(handle_key(key(KeyCode::Char('r'))), Action::CycleRole);
    }

    #[test]
    fn test_navigation_keys() {
        assert_eq!(handle_key(key(KeyCode::Char('j'))), Action::SelectNext);
        assert_eq!(handle_key(key(KeyCode::Down)), Action::SelectNext);
        assert_eq!(handle_key(key(KeyCode::Char('k'))), Action::SelectPrev);
        assert_eq!(handle_key(key(KeyCode::Up)), Action::SelectPrev);
    }

    #[test]
    fn test_tab_keys() {
        assert_eq!(handle_key(key(KeyCode::Tab)), Action::NextView);
        assert_eq!(
            handle_key(key_with_mods(KeyCode::Tab, KeyModifiers::SHIFT)),
            Action::PrevView
        );
        assert_eq!(handle_key(key(KeyCode::BackTab)), Action::PrevView);
    }

    #[test]
    fn test_unknown_key() {
        assert_eq!(handle_key(key(KeyCode::Char('x'))), Action::None);
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
}
