//! TUI Client for Loopr
//!
//! Terminal user interface for interacting with the Loopr daemon.
//! Provides chat view, loops view, and plan approval interfaces.

pub mod app;
pub mod input;
pub mod views;

pub use app::App;
pub use input::{InputHandler, KeyEvent};
pub use views::{ApprovalView, ChatView, LoopsView, View};
