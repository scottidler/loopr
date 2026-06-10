//! Domain records, FSM const transition tables, TaskStore wrapper. Pure data and invariants.

// The `Fsm` derive emits fully-qualified `::domain::Transition` etc. paths so
// external consumers need no `use` statement. From inside `domain` itself,
// `::domain::*` does not resolve without a crate alias. This line registers
// the crate as `domain` for its own macro expansions. Do not remove.
extern crate self as domain;

mod bundle;
mod criteria;
mod failure;
mod fsm;
mod graph;
mod id;
mod note;
mod plan;
mod role;
mod tick;
mod verdict;
mod work;

pub use bundle::{Bundle, BundleStatus};
pub use criteria::AcceptanceCriteria;
pub use failure::FailureReason;
pub use fsm::{FsmError, FsmErrorKind, TargetKind, Transition};
pub use graph::{GraphError, WorkGraph};
pub use id::{BundleId, NoteId, PlanId, TickId, WorkId, generate_id, now_millis};
pub use note::OperatorNote;
pub use plan::{Plan, PlanStatus};
pub use role::Role;
pub use tick::Tick;
pub use verdict::{ReviewIssue, Severity, Verdict};
pub use work::{Work, WorkStatus};
