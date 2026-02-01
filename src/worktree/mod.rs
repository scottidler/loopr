//! Git worktree management for loop isolation.
//!
//! Each loop gets its own worktree with a dedicated branch, providing
//! complete isolation between concurrent loops.

mod manager;

pub use manager::WorktreeManager;
