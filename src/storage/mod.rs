//! Storage layer for Loopr - JSONL-based persistence with in-memory caching.
//!
//! This module provides the storage abstraction for persisting Loop, Signal,
//! ToolJob, and Event records.

mod traits;
mod jsonl;
mod loops;

pub use traits::{Filter, FilterOp, HasId, Storage};
pub use jsonl::JsonlStorage;
pub use loops::LoopStore;
