//! Domain types for Loopr
//!
//! This module contains all core domain types:
//! - Loop: The central Loop record with identity, artifacts, config, and state
//! - Signal: Inter-loop communication (stop, pause, invalidate)
//! - ToolJob: Tool execution records
//! - Event: Audit/history events

pub mod event;
pub mod loop_record;
pub mod signal;
pub mod tool_job;

pub use event::{event_types, EventRecord};
pub use loop_record::{Loop, LoopStatus, LoopType};
pub use signal::{SignalRecord, SignalType};
pub use tool_job::{ToolJobRecord, ToolJobStatus};
