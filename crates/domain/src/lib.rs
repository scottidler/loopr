//! Domain records, FSM const transition tables, TaskStore wrapper. Pure data and invariants.

mod fsm;
mod role;

pub use fsm::{FsmError, FsmErrorKind, TargetKind, Transition};
pub use role::Role;
