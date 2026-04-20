//! Domain records, FSM const transition tables, TaskStore wrapper. Pure data and invariants.

mod fsm;
mod id;
mod role;

pub use fsm::{FsmError, FsmErrorKind, TargetKind, Transition};
pub use id::{PlanId, generate_id, now_millis};
pub use role::Role;
