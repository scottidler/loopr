//! Exhaustive FSM correctness tests, ported to the runtime interpreter.
//!
//! These tests systematically prove:
//! 1. Every valid transition succeeds with the correct role
//! 2. Every invalid transition is rejected (wrong role, skip state, terminal, reverse, self)
//! 3. Terminal states cannot transition to ANY other state
//! 4. Self-transitions are idempotent (return Transition::Unchanged)
//! 5. Records serialize/deserialize correctly through the full lifecycle
//!
//! Organized by FSM, with N×N matrix coverage for each.

mod common;

mod bundle;
mod dispatch;
mod hierarchy;
mod lock;
mod status;
mod tick;
mod work;
