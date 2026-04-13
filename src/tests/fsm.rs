//! FSM correctness tests.
//!
//! The exhaustive transition matrix tests are now in `src/fsm/tests.rs` (runtime interpreter).
//! This module retains:
//! - `dispatch` - tests domain type `transition()` methods via the interpreter
//! - `lock` - LockStatus (imperative, not YAML-driven)
//! - `status` - AgentStatus (imperative, not derive-driven)

mod common;

mod dispatch;
mod lock;
mod status;
