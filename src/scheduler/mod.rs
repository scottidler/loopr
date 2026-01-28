// TODO: Remove these allows once scheduler is integrated with TUI/main
#![allow(dead_code)]
#![allow(unused_imports)]

//! Scheduler module for loop prioritization and execution management.
//!
//! This module provides:
//! - **Priority calculation**: Determines which loops should run first based on
//!   type, age, hierarchy depth, and retry history.
//! - **Scheduler**: Selects runnable loops respecting dependencies and concurrency limits.
//! - **Rate limiting**: Coordinated backoff when the LLM API returns rate limit errors.
//! - **LoopManager**: Orchestrates loop execution with polling-based coordination.
//!
//! # Architecture
//!
//! The scheduler uses a polling model:
//! 1. LoopManager polls TaskStore for pending loops
//! 2. Scheduler calculates priorities and selects runnable loops
//! 3. Selected loops are spawned as async tasks
//! 4. Loops report events back via channels
//!
//! # Example
//!
//! ```ignore
//! use loopr::scheduler::{LoopManager, LoopManagerConfig};
//! use loopr::store::TaskStore;
//!
//! let store = TaskStore::open_at(Path::new("/tmp/test"))?;
//! let mut manager = LoopManager::new(store);
//!
//! // Run the manager's polling loop
//! manager.run().await?;
//! ```

mod manager;
mod priority;
mod rate_limit;
mod select;

pub use manager::{LoopEvent, LoopManager, LoopManagerConfig};
pub use priority::{
    AGE_BOOST_MAX, AGE_BOOST_PER_MINUTE, DEPTH_BOOST_PER_LEVEL, PRIORITY_PHASE, PRIORITY_PLAN, PRIORITY_RALPH,
    PRIORITY_SPEC, PriorityConfig, RETRY_PENALTY_MAX, RETRY_PENALTY_PER_ITERATION, base_priority, calculate_depth,
    calculate_priority, is_runnable,
};
pub use rate_limit::{RateLimitConfig, RateLimitState};
pub use select::{ConcurrencyConfig, Scheduler};
