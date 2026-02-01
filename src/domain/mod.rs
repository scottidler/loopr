//! Domain types for Loopr
//!
//! This module contains all core domain types:
//! - Loop: The central Loop record with identity, artifacts, config, and state
//! - LoopOutcome: Result of loop execution (Complete, Failed, Invalidated)
//! - Signal: Inter-loop communication (stop, pause, invalidate)
//! - ToolJob: Tool execution records
//! - Event: Audit/history events
//!
//! Per domain-types.md, Loop is self-contained with its own `run()` method.
//! There is no separate LoopRunner - that was unnecessary indirection.

pub mod event;
pub mod loop_record;
pub mod outcome;
pub mod signal;
pub mod tool_job;

pub use event::{EventRecord, event_types};
pub use loop_record::{Loop, LoopRunConfig, LoopStatus, LoopType};
pub use outcome::LoopOutcome;
pub use signal::{SignalRecord, SignalType};
pub use tool_job::{ToolJobRecord, ToolJobStatus};
