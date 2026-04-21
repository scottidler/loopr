//! Domain records, FSM const transition tables, TaskStore wrapper. Pure data and invariants.

// The `Fsm` derive emits fully-qualified `::domain::Transition` etc. paths so
// external consumers need no `use` statement. From inside `domain` itself,
// `::domain::*` does not resolve without a crate alias. This line registers
// the crate as `domain` for its own macro expansions. Do not remove.
extern crate self as domain;

mod criteria;
mod fsm;
mod id;
mod plan;
mod role;
mod work;

pub use criteria::AcceptanceCriteria;
pub use fsm::{FsmError, FsmErrorKind, TargetKind, Transition};
pub use id::{PlanId, WorkId, generate_id, now_millis};
pub use plan::{Plan, PlanStatus};
pub use role::Role;
pub use work::{Work, WorkStatus};
