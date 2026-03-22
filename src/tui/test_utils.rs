#![cfg(test)]
#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::app::App;
use super::input::{apply_action, handle_key};

/// Create a test terminal with the given dimensions.
pub fn test_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(width, height)).expect("failed to create test terminal")
}

/// Check if a buffer contains the given text substring anywhere.
///
/// Scans row-by-row, concatenating cell symbols into a string per row,
/// then checks if any row contains `text` as a substring.
pub fn buffer_contains_text(buffer: &Buffer, text: &str) -> bool {
    let area = buffer.area;
    for y in area.top()..area.bottom() {
        let mut row = String::new();
        for x in area.left()..area.right() {
            row.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
        }
        if row.contains(text) {
            return true;
        }
    }
    false
}

/// Extract all text from a buffer region, one string per row.
/// Trailing whitespace is trimmed from each row.
pub fn buffer_text_rows(buffer: &Buffer, area: Rect) -> Vec<String> {
    (area.top()..area.bottom())
        .map(|y| {
            let mut row = String::new();
            for x in area.left()..area.right() {
                row.push_str(buffer.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            row.trim_end().to_string()
        })
        .collect()
}

/// Simulate typing a string and pressing Enter.
pub fn type_and_submit(app: &mut App, text: &str) {
    for c in text.chars() {
        let action = handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), app.input_mode);
        apply_action(app, action);
    }
    let action = handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), app.input_mode);
    apply_action(app, action);
}

/// Simulate a single key press.
pub fn press_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let action = handle_key(KeyEvent::new(code, modifiers), app.input_mode);
    apply_action(app, action);
}
