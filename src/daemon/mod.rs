//! Daemon Core - scheduler, tick loop, and crash recovery
//!
//! The daemon is the long-running process that:
//! - Schedules and executes loops based on priority
//! - Runs a tick loop to process pending work
//! - Recovers from crashes by restoring interrupted loops

pub mod recovery;
pub mod scheduler;
pub mod tick;

pub use recovery::*;
pub use scheduler::*;
pub use tick::*;
