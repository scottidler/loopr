//! `Work` record type and `WorkStatus` state machine.
//!
//! A `Work` is a leaf-level implementation unit decomposed from a
//! `Plan`. Stage 6 introduces the type; Stage 7's reactive coordinator
//! drives the FSM through Pending -> Ready -> InProgress -> InReview
//! -> Integrated -> Done as deps clear, agents run, and Bundles merge.
//!
//! Display output on `WorkStatus` is lowercase to match the serde wire
//! form — the `Record` derive calls `ToString::to_string` on indexed
//! fields, so the index map value and the on-disk JSON value must use
//! the same spelling.
//!
//! The Phase 2 scope of this file is the `WorkStatus` enum only; the
//! `Work` struct and its constructor/transition methods land in Phase
//! 3 of hierarchy.md.

use serde::{Deserialize, Serialize};
use strum::Display;

use derive::{Fsm, Record};

use crate::criteria::AcceptanceCriteria;
use crate::id::{PlanId, WorkId, now_millis};
use crate::{FsmError, Role, Transition};

/// Lifecycle state for `Work`. Ports v4's 10-state `work.yml` table,
/// with round-2 Architect adjustments applied: `Integrated =>
/// Superseded` added so a superseded parent `Plan` can cascade-cancel
/// Works mid-integration; `Ready => Done` demoted from the routine
/// transitions table to `overrides(...)` so the no-op-Work bypass
/// cannot be used as an AC-skipping loophole on the normal path;
/// `Integrated => Done` is `Coordinator`-only because the "no active
/// sessions" guard needs daemon-level visibility the `integrator`
/// crate does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, Fsm)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[fsm(
    role = crate::Role,
    terminal = [Done, Superseded, Abandoned],
    transitions(
        Draft       => Pending    by (Coordinator),
        Draft       => Ready      by (Coordinator),
        Draft       => Superseded by (Coordinator, Director),
        Draft       => Abandoned  by (Coordinator, Director),
        Pending     => Ready      by (Coordinator),
        Pending     => Superseded by (Coordinator, Director),
        Pending     => Abandoned  by (Coordinator, Director),
        Ready       => InProgress by (Coordinator),
        Ready       => Blocked    by (Coordinator),
        Ready       => Superseded by (Coordinator, Director),
        Ready       => Abandoned  by (Coordinator, Director),
        InProgress  => Blocked    by (Coordinator, Implementer),
        InProgress  => InReview   by (Implementer),
        InProgress  => Superseded by (Coordinator, Director),
        InProgress  => Abandoned  by (Coordinator, Director),
        Blocked     => Ready      by (Coordinator),
        Blocked     => Superseded by (Coordinator, Director),
        Blocked     => Abandoned  by (Coordinator, Director),
        InReview    => InProgress by (Coordinator),
        InReview    => Integrated by (Integrator),
        InReview    => Superseded by (Coordinator, Director),
        InReview    => Abandoned  by (Coordinator, Director),
        Integrated  => Done       by (Coordinator),
        Integrated  => Superseded by (Coordinator, Director),
        Integrated  => Abandoned  by (Coordinator, Director),
    ),
    overrides(
        Ready      => Done     by (Coordinator),
        InProgress => Ready    by (Coordinator),
        InProgress => InReview by (Coordinator),
        InReview   => Ready    by (Coordinator),
        InReview   => Blocked  by (Coordinator),
    ),
)]
pub enum WorkStatus {
    Draft,
    Pending,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Integrated,
    Done,
    Superseded,
    Abandoned,
}

/// Leaf-level implementation unit decomposed from a `Plan`. Persisted
/// at `<target>/.loopr/taskstore/works.jsonl` via the `Record` derive.
///
/// Fields ported from v3's `Work` post-v0.1.96 (the
/// description-field-crisis remediation) with two v5 upgrades: typed
/// `WorkId` / `PlanId` instead of `String`, and indexed `parent_id`
/// for Stage 7's reactive coordinator (which scans "child Works of
/// this Plan" on every tick). `attempt_count`, `session_failure_count`,
/// `files`, and `assignee` ship with `Default` values; Stage 7
/// populates them when the coordinator and reviewer wire up. No
/// `description` field (v0.1.96 removed it); no `blocked_reason`
/// (deferred per scope memo D3).
#[derive(Debug, Clone, Serialize, Deserialize, Record)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub id: WorkId,
    #[record(indexed)]
    pub parent_id: PlanId,
    pub updated_at: i64,
    pub created_at: i64,
    pub title: String,
    #[serde(default)]
    pub assignee: Option<String>,
    #[record(indexed)]
    pub status: WorkStatus,
    #[serde(default)]
    pub dependencies: Vec<WorkId>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    #[serde(default)]
    pub attempt_count: u32,
    #[serde(default)]
    pub session_failure_count: u32,
}

impl Work {
    /// New Work under the given Plan. Status starts `Pending` (not
    /// `Draft`) because the reactive-execution convention is that a
    /// freshly decomposed Work is immediately eligible for the
    /// Coordinator's `Pending -> Ready` transition once its deps
    /// clear. Stage 6's decomposer never constructs a `Draft` Work;
    /// `Draft` is reserved for a future pre-decomposition authoring
    /// flow that would want a different constructor.
    pub fn new(parent_id: PlanId, title: String) -> Self {
        let now = now_millis();
        Self {
            id: WorkId::new(),
            parent_id,
            updated_at: now,
            created_at: now,
            title,
            assignee: None,
            status: WorkStatus::Pending,
            dependencies: Vec::new(),
            files: Vec::new(),
            acceptance_criteria: AcceptanceCriteria::default(),
            attempt_count: 0,
            session_failure_count: 0,
        }
    }

    /// Read current status. The field is `pub`; this method exists
    /// for method-chain call sites.
    pub fn status(&self) -> WorkStatus {
        self.status
    }

    /// Validated FSM transition. Delegates to the Fsm-derived
    /// `validate_transition`. On any state-changing result (`Changed`),
    /// updates `self.status` and `self.updated_at`. `Unchanged`
    /// (from == to) leaves state intact. Invalid transitions return
    /// `FsmError`.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(record_kind = "work", record_id = %self.id, from = ?self.status, target = ?target, role = ?role),
        ret,
        err,
    )]
    pub fn transition(&mut self, target: WorkStatus, role: Role) -> Result<Transition, FsmError<WorkStatus>> {
        let result = WorkStatus::validate_transition(self.status, target, role)?;
        if result != Transition::Unchanged {
            self.status = target;
            self.updated_at = now_millis();
        }
        Ok(result)
    }

    /// Validated FSM override. Delegates to the Fsm-derived
    /// `validate_override`, which itself tries `validate_transition`
    /// first and falls through to the override table only on
    /// rejection. Any state-changing result (`Changed` or `Override`)
    /// updates `self.status` and `self.updated_at`; only `Unchanged`
    /// leaves state intact.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(record_kind = "work", record_id = %self.id, from = ?self.status, target = ?target, role = ?role, override_ = true),
        ret,
        err,
    )]
    pub fn override_status(&mut self, target: WorkStatus, role: Role) -> Result<Transition, FsmError<WorkStatus>> {
        let result = WorkStatus::validate_override(self.status, target, role)?;
        if result != Transition::Unchanged {
            self.status = target;
            self.updated_at = now_millis();
        }
        Ok(result)
    }
}
