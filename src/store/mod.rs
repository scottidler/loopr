// TODO: Remove these allows once store is used by other modules
#![allow(dead_code)]
#![allow(unused_imports)]

//! Storage layer for Loopr.
//!
//! This module provides persistence for loop records using a dual-storage approach:
//! - **JSONL file**: Append-only log (source of truth)
//! - **SQLite database**: Query index (rebuilt from JSONL)
//!
//! # Example
//!
//! ```ignore
//! use loopr::store::{TaskStore, LoopRecord, LoopType, LoopStatus};
//! use std::path::Path;
//!
//! // Open store for a project
//! let mut store = TaskStore::open(Path::new("/path/to/project"))?;
//!
//! // Create and save a new loop
//! let record = LoopRecord::new_plan("Build a REST API", 15);
//! store.save(&record)?;
//!
//! // Query loops
//! let pending = store.list_by_status(LoopStatus::Pending)?;
//! let plans = store.list_by_type(LoopType::Plan)?;
//! ```

mod records;
mod task_store;

pub use records::{IndexValue, LoopRecord, LoopStatus, LoopType, generate_loop_id, now_ms};
pub use task_store::{TaskStore, compute_project_hash};
