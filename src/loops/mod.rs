//! Loop execution module for Loopr.
//!
//! This module provides the core loop execution logic including:
//! - `RalphLoop`: The leaf-level loop that does actual coding work
//! - `Worktree`: Git worktree management for isolated execution
//! - `Validation`: Basic validation (run command, check exit code)
//!
//! Note: dead_code/unused warnings are expected during Phase 3 development.
//! These will be cleaned up when the module is integrated in later phases.

#![allow(dead_code)]
#![allow(unused_imports)]

mod ralph;
mod validation;
mod worktree;

pub use ralph::{IterationResult, LoopAction, LoopError, LoopRunner, RalphLoop, RalphLoopConfig};
pub use validation::{ValidationConfig, ValidationFeedback, ValidationResult, Validator};
pub use worktree::{Worktree, WorktreeConfig, WorktreeError};
