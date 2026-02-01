//! Storage layer for Loopr - JSONL-based persistence with in-memory caching.
//!
//! This module provides the storage abstraction for persisting Loop, Signal,
//! ToolJob, and Event records.

mod jsonl;
mod loops;
mod traits;

pub use jsonl::JsonlStorage;
pub use loops::LoopStore;
pub use traits::{Filter, FilterOp, HasId, Storage};
