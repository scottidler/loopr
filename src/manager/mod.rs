//! Loop Manager module
//!
//! Orchestrates loop lifecycle - creation, execution, child spawning.

mod loop_manager;
mod spawner;

pub use loop_manager::{LoopManager, LoopManagerConfig};
pub use spawner::{ChildSpawner, SpawnDecision};
