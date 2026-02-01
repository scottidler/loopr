//! Loopr - A loop-based task orchestration system
//!
//! Loopr implements the "Ralph Wiggum" pattern for AI-assisted software development,
//! where loops iterate with fresh context until validation passes.

pub mod artifact;
pub mod coordination;
pub mod domain;
pub mod error;
pub mod id;
pub mod llm;
pub mod prompt;
pub mod runner;
pub mod storage;
pub mod tools;
pub mod validation;
pub mod worktree;

pub use error::{LooprError, Result};
