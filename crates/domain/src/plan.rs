//! `Plan` record type and `PlanStatus` state machine.
//!
//! A `Plan` is the top-of-hierarchy objective a user files via
//! `loopr plan "..."`. Stage 5 carries the minimum field set: `id`,
//! `updated_at`, `created_at`, `goal`, `status`. Later stages layer
//! parent/child relations, acceptance criteria, tier, and decomposer
//! state onto record types introduced there.

use serde::{Deserialize, Serialize};
use strum::Display;

use derive::{Fsm, Record};

use crate::id::{PlanId, now_millis};
use crate::{FsmError, Role, Transition};

/// Lifecycle state for `Plan`. Copies v4's proven `hierarchy.yml` transition
/// table (Draft/Pending/Active terminalized to Complete/Superseded/Abandoned).
///
/// Display output is lowercase to match the serde wire form — the Record
/// derive calls `ToString::to_string` on indexed fields, so the index map
/// value and the on-disk JSON value must use the same spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, Fsm)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[fsm(
    role = crate::Role,
    terminal = [Complete, Superseded, Abandoned],
    transitions(
        Draft   => Pending    by (Coordinator),
        Draft   => Active     by (Coordinator),
        Draft   => Superseded by (Coordinator, Director),
        Draft   => Abandoned  by (Coordinator, Director),
        Pending => Active     by (Coordinator),
        Pending => Superseded by (Coordinator, Director),
        Pending => Abandoned  by (Coordinator, Director),
        Active  => Complete   by (Coordinator, Decomposer),
        Active  => Superseded by (Coordinator, Director),
        Active  => Abandoned  by (Coordinator, Director),
    ),
    overrides(
        Active  => Draft by (Director),
        Pending => Draft by (Director),
    ),
)]
pub enum PlanStatus {
    Draft,
    Pending,
    Active,
    Complete,
    Superseded,
    Abandoned,
}

/// User-filed objective. Persisted at `<target>/.loopr/taskstore/plans.jsonl`
/// via the `Record` derive.
#[derive(Debug, Clone, Serialize, Deserialize, Record)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub id: PlanId,
    pub updated_at: i64,
    pub created_at: i64,
    pub goal: String,
    #[record(indexed)]
    pub status: PlanStatus,
}

impl Plan {
    /// New Plan: fresh PlanId, status = Active, created_at = updated_at = now.
    ///
    /// Stage 5 has no clarity / interview loop; a user-filed goal is
    /// immediately Active. Later stages may reintroduce Draft as the birth
    /// state by changing this one line.
    pub fn new(goal: String) -> Self {
        let now = now_millis();
        Self {
            id: PlanId::new(),
            updated_at: now,
            created_at: now,
            goal,
            status: PlanStatus::Active,
        }
    }

    /// Read current status. The field is `pub`; this method exists for
    /// method-chain call sites.
    pub fn status(&self) -> PlanStatus {
        self.status
    }

    /// Validated FSM transition. Delegates to the Fsm-derived
    /// `validate_transition`. On any state-changing result (`Changed`),
    /// updates `self.status` and `self.updated_at`. `Unchanged` (from == to)
    /// leaves state intact. Invalid transitions return `FsmError`.
    pub fn transition(&mut self, target: PlanStatus, role: Role) -> Result<Transition, FsmError<PlanStatus>> {
        let result = PlanStatus::validate_transition(self.status, target, role)?;
        if result != Transition::Unchanged {
            self.status = target;
            self.updated_at = now_millis();
        }
        Ok(result)
    }

    /// Validated FSM override. Delegates to the Fsm-derived
    /// `validate_override`, which itself tries `validate_transition` first
    /// and falls through to the override table only on rejection. Any
    /// state-changing result (`Changed` or `Override`) updates `self.status`
    /// and `self.updated_at`; only `Unchanged` leaves state intact.
    pub fn override_status(&mut self, target: PlanStatus, role: Role) -> Result<Transition, FsmError<PlanStatus>> {
        let result = PlanStatus::validate_override(self.status, target, role)?;
        if result != Transition::Unchanged {
            self.status = target;
            self.updated_at = now_millis();
        }
        Ok(result)
    }
}
