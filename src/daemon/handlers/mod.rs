//! Request handlers for the daemon
//!
//! Each submodule handles a category of IPC methods:
//! - loops: Loop lifecycle (list, get, create, start, pause, resume, cancel, delete)
//! - chat: Chat functionality (send, clear)
//! - plan: Plan approval flow (approve, reject, iterate, preview)

pub mod chat;
pub mod loops;
pub mod plan;

pub use chat::*;
pub use loops::*;
pub use plan::*;
