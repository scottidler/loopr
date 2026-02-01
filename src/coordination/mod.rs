//! Loop coordination module
//!
//! Implements signal-based coordination between loops including
//! stop, pause, resume, and invalidation cascade.

pub mod invalidate;
pub mod signals;

pub use invalidate::*;
pub use signals::*;
