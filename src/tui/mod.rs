//! Terminal User Interface for Loopr.
//!
//! This module provides a k9s-style terminal interface with two views:
//! - **Chat**: Conversation with LLM, plan creation
//! - **Loops**: Hierarchical tree of running loops
//!
//! The TUI runs as part of the main process using tokio for async operations.

mod app;
mod events;
mod runner;
mod state;
mod tree;
mod views;

#[allow(unused_imports)]
pub use app::App;
#[allow(unused_imports)]
pub use events::{Event, EventHandler};
pub use runner::TuiRunner;
#[allow(unused_imports)]
pub use state::{AppState, InteractionMode, View};
#[allow(unused_imports)]
pub use tree::{LoopItem, LoopTree, TreeNode};

use crossterm::{
    ExecutableCommand,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use eyre::Result;
use ratatui::prelude::*;
use std::io::{Stdout, stdout};

/// Type alias for our terminal backend.
pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Initialize the terminal for TUI mode.
///
/// Enables raw mode and switches to the alternate screen.
pub fn init_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
///
/// Disables raw mode and leaves the alternate screen.
pub fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

/// Status colors inspired by k9s.
pub mod colors {
    use ratatui::style::Color;

    pub const RUNNING: Color = Color::Rgb(0, 255, 127); // Spring green
    pub const PENDING: Color = Color::Rgb(255, 215, 0); // Gold
    pub const COMPLETE: Color = Color::Rgb(50, 205, 50); // Lime green
    pub const FAILED: Color = Color::Rgb(220, 20, 60); // Crimson
    pub const DRAFT: Color = Color::Rgb(255, 255, 0); // Yellow
    pub const HEADER: Color = Color::Rgb(0, 255, 255); // Cyan
    pub const KEYBIND: Color = Color::Rgb(0, 255, 255); // Cyan
    pub const DIM: Color = Color::DarkGray;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_colors_defined() {
        // Just verify colors module is accessible
        let _ = colors::RUNNING;
        let _ = colors::PENDING;
        let _ = colors::COMPLETE;
        let _ = colors::FAILED;
    }
}
