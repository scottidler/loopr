//! Input handling for TUI
//!
//! Handles keyboard input and converts to commands.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use std::time::Duration;

/// Key event representation
#[derive(Debug, Clone, PartialEq)]
pub struct KeyEvent {
    /// The key code
    pub code: KeyCode,
    /// Modifier keys held
    pub modifiers: KeyModifiers,
}

impl KeyEvent {
    /// Create a new key event
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Check if this is a quit key (q or Ctrl+C)
    pub fn is_quit(&self) -> bool {
        self.code == KeyCode::Char('q')
            || (self.code == KeyCode::Char('c') && self.modifiers.contains(KeyModifiers::CONTROL))
    }

    /// Check if this is the escape key
    pub fn is_escape(&self) -> bool {
        self.code == KeyCode::Esc
    }

    /// Check if this is the enter key
    pub fn is_enter(&self) -> bool {
        self.code == KeyCode::Enter
    }

    /// Check if this is the tab key
    pub fn is_tab(&self) -> bool {
        self.code == KeyCode::Tab
    }

    /// Check if this is the up arrow key
    pub fn is_up(&self) -> bool {
        self.code == KeyCode::Up
    }

    /// Check if this is the down arrow key
    pub fn is_down(&self) -> bool {
        self.code == KeyCode::Down
    }

    /// Check if this is the left arrow key
    pub fn is_left(&self) -> bool {
        self.code == KeyCode::Left
    }

    /// Check if this is the right arrow key
    pub fn is_right(&self) -> bool {
        self.code == KeyCode::Right
    }

    /// Check if this is a character key
    pub fn is_char(&self) -> bool {
        matches!(self.code, KeyCode::Char(_))
    }

    /// Get the character if this is a char key
    pub fn char(&self) -> Option<char> {
        if let KeyCode::Char(c) = self.code { Some(c) } else { None }
    }

    /// Check if this is the backspace key
    pub fn is_backspace(&self) -> bool {
        self.code == KeyCode::Backspace
    }

    /// Check if this is the delete key
    pub fn is_delete(&self) -> bool {
        self.code == KeyCode::Delete
    }

    /// Check if this is the home key
    pub fn is_home(&self) -> bool {
        self.code == KeyCode::Home
    }

    /// Check if this is the end key
    pub fn is_end(&self) -> bool {
        self.code == KeyCode::End
    }
}

/// Input handler for the TUI
pub struct InputHandler {
    /// Poll timeout duration
    poll_timeout: Duration,
}

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl InputHandler {
    /// Create a new input handler
    pub fn new() -> Self {
        Self {
            poll_timeout: Duration::from_millis(100),
        }
    }

    /// Create with custom poll timeout
    pub fn with_timeout(timeout: Duration) -> Self {
        Self { poll_timeout: timeout }
    }

    /// Poll for the next key event
    pub fn poll(&self) -> std::io::Result<Option<KeyEvent>> {
        if event::poll(self.poll_timeout)?
            && let Event::Key(key) = event::read()?
        {
            return Ok(Some(KeyEvent::new(key.code, key.modifiers)));
        }
        Ok(None)
    }

    /// Read the next key event (blocking)
    pub fn read(&self) -> std::io::Result<KeyEvent> {
        loop {
            if let Event::Key(key) = event::read()? {
                return Ok(KeyEvent::new(key.code, key.modifiers));
            }
        }
    }

    /// Get the poll timeout
    pub fn poll_timeout(&self) -> Duration {
        self.poll_timeout
    }

    /// Set the poll timeout
    pub fn set_poll_timeout(&mut self, timeout: Duration) {
        self.poll_timeout = timeout;
    }
}

/// Text input buffer for handling text entry
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    /// The text content
    content: String,
    /// Cursor position
    cursor: usize,
}

impl TextInput {
    /// Create a new empty text input
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with initial content
    pub fn with_content(content: &str) -> Self {
        let len = content.len();
        Self {
            content: content.to_string(),
            cursor: len,
        }
    }

    /// Get the content
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Get the cursor position
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Insert a character at the cursor
    pub fn insert(&mut self, c: char) {
        self.content.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character before the cursor
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev_char_boundary = self.prev_char_boundary(self.cursor);
            self.content.remove(prev_char_boundary);
            self.cursor = prev_char_boundary;
        }
    }

    /// Delete the character at the cursor
    pub fn delete(&mut self) {
        if self.cursor < self.content.len() {
            self.content.remove(self.cursor);
        }
    }

    /// Move cursor left
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.prev_char_boundary(self.cursor);
        }
    }

    /// Move cursor right
    pub fn move_right(&mut self) {
        if self.cursor < self.content.len() {
            self.cursor = self.next_char_boundary(self.cursor);
        }
    }

    /// Move cursor to start
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move cursor to end
    pub fn move_end(&mut self) {
        self.cursor = self.content.len();
    }

    /// Clear the content
    pub fn clear(&mut self) {
        self.content.clear();
        self.cursor = 0;
    }

    /// Take the content and clear
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.content)
    }

    /// Handle a key event
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c) => {
                self.insert(c);
                true
            }
            KeyCode::Backspace => {
                self.backspace();
                true
            }
            KeyCode::Delete => {
                self.delete();
                true
            }
            KeyCode::Left => {
                self.move_left();
                true
            }
            KeyCode::Right => {
                self.move_right();
                true
            }
            KeyCode::Home => {
                self.move_home();
                true
            }
            KeyCode::End => {
                self.move_end();
                true
            }
            _ => false,
        }
    }

    /// Find the previous character boundary
    fn prev_char_boundary(&self, pos: usize) -> usize {
        let mut idx = pos.saturating_sub(1);
        while idx > 0 && !self.content.is_char_boundary(idx) {
            idx -= 1;
        }
        idx
    }

    /// Find the next character boundary
    fn next_char_boundary(&self, pos: usize) -> usize {
        let mut idx = pos + 1;
        while idx < self.content.len() && !self.content.is_char_boundary(idx) {
            idx += 1;
        }
        idx.min(self.content.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_event_is_quit() {
        let q_key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(q_key.is_quit());

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(ctrl_c.is_quit());

        let a_key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(!a_key.is_quit());
    }

    #[test]
    fn test_key_event_is_escape() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(esc.is_escape());

        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!q.is_escape());
    }

    #[test]
    fn test_key_event_is_enter() {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(enter.is_enter());
    }

    #[test]
    fn test_key_event_is_tab() {
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert!(tab.is_tab());
    }

    #[test]
    fn test_key_event_arrows() {
        assert!(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE).is_up());
        assert!(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE).is_down());
        assert!(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE).is_left());
        assert!(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE).is_right());
    }

    #[test]
    fn test_key_event_char() {
        let a_key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(a_key.is_char());
        assert_eq!(a_key.char(), Some('a'));

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!enter.is_char());
        assert_eq!(enter.char(), None);
    }

    #[test]
    fn test_key_event_special_keys() {
        assert!(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE).is_backspace());
        assert!(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE).is_delete());
        assert!(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE).is_home());
        assert!(KeyEvent::new(KeyCode::End, KeyModifiers::NONE).is_end());
    }

    #[test]
    fn test_input_handler_new() {
        let handler = InputHandler::new();
        assert_eq!(handler.poll_timeout(), Duration::from_millis(100));
    }

    #[test]
    fn test_input_handler_with_timeout() {
        let handler = InputHandler::with_timeout(Duration::from_millis(50));
        assert_eq!(handler.poll_timeout(), Duration::from_millis(50));
    }

    #[test]
    fn test_input_handler_set_timeout() {
        let mut handler = InputHandler::new();
        handler.set_poll_timeout(Duration::from_millis(200));
        assert_eq!(handler.poll_timeout(), Duration::from_millis(200));
    }

    #[test]
    fn test_text_input_new() {
        let input = TextInput::new();
        assert_eq!(input.content(), "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn test_text_input_with_content() {
        let input = TextInput::with_content("hello");
        assert_eq!(input.content(), "hello");
        assert_eq!(input.cursor(), 5);
    }

    #[test]
    fn test_text_input_insert() {
        let mut input = TextInput::new();
        input.insert('h');
        input.insert('i');
        assert_eq!(input.content(), "hi");
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn test_text_input_backspace() {
        let mut input = TextInput::with_content("hello");
        input.backspace();
        assert_eq!(input.content(), "hell");
        assert_eq!(input.cursor(), 4);
    }

    #[test]
    fn test_text_input_backspace_at_start() {
        let mut input = TextInput::new();
        input.backspace();
        assert_eq!(input.content(), "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn test_text_input_delete() {
        let mut input = TextInput::with_content("hello");
        input.move_home();
        input.delete();
        assert_eq!(input.content(), "ello");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn test_text_input_move_left_right() {
        let mut input = TextInput::with_content("hello");
        input.move_left();
        assert_eq!(input.cursor(), 4);
        input.move_right();
        assert_eq!(input.cursor(), 5);
    }

    #[test]
    fn test_text_input_move_home_end() {
        let mut input = TextInput::with_content("hello");
        input.move_home();
        assert_eq!(input.cursor(), 0);
        input.move_end();
        assert_eq!(input.cursor(), 5);
    }

    #[test]
    fn test_text_input_clear() {
        let mut input = TextInput::with_content("hello");
        input.clear();
        assert_eq!(input.content(), "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn test_text_input_take() {
        let mut input = TextInput::with_content("hello");
        let content = input.take();
        assert_eq!(content, "hello");
        assert_eq!(input.content(), "");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn test_text_input_handle_key_char() {
        let mut input = TextInput::new();
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(input.handle_key(&key));
        assert_eq!(input.content(), "a");
    }

    #[test]
    fn test_text_input_handle_key_backspace() {
        let mut input = TextInput::with_content("hi");
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(input.handle_key(&key));
        assert_eq!(input.content(), "h");
    }

    #[test]
    fn test_text_input_handle_key_arrows() {
        let mut input = TextInput::with_content("hi");
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert!(input.handle_key(&left));
        assert_eq!(input.cursor(), 1);

        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert!(input.handle_key(&right));
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn test_text_input_handle_key_home_end() {
        let mut input = TextInput::with_content("hello");
        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
        assert!(input.handle_key(&home));
        assert_eq!(input.cursor(), 0);

        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        assert!(input.handle_key(&end));
        assert_eq!(input.cursor(), 5);
    }

    #[test]
    fn test_text_input_handle_key_unhandled() {
        let mut input = TextInput::new();
        let key = KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE);
        assert!(!input.handle_key(&key));
    }

    #[test]
    fn test_text_input_insert_in_middle() {
        let mut input = TextInput::with_content("hllo");
        input.cursor = 1;
        input.insert('e');
        assert_eq!(input.content(), "hello");
        assert_eq!(input.cursor(), 2);
    }
}
