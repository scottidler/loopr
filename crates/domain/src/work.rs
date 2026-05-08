//! `Work` record type and `WorkStatus` state machine.
//!
//! A `Work` is a leaf-level implementation unit decomposed from a
//! `Plan`. Stage 6 introduces the type; Stage 7's Reactor
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
/// `Integrated => Done` is `Reactor`-only because the "no active
/// sessions" guard needs daemon-level visibility the `integrator`
/// crate does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, Fsm)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[fsm(
    role = crate::Role,
    terminal = [Done, Superseded, Abandoned],
    transitions(
        Draft       => Pending    by (Reactor),
        Draft       => Ready      by (Reactor),
        Draft       => Superseded by (Reactor, Director),
        Draft       => Abandoned  by (Reactor, Director),
        Pending     => Ready      by (Reactor),
        Pending     => Blocked    by (Reactor),
        Pending     => Superseded by (Reactor, Director),
        Pending     => Abandoned  by (Reactor, Director),
        Ready       => InProgress by (Reactor),
        Ready       => Blocked    by (Reactor),
        Ready       => Superseded by (Reactor, Director),
        Ready       => Abandoned  by (Reactor, Director),
        InProgress  => Blocked    by (Reactor, Implementer),
        InProgress  => InReview   by (Implementer),
        InProgress  => Superseded by (Reactor, Director),
        InProgress  => Abandoned  by (Reactor, Director),
        Blocked     => Ready      by (Reactor),
        Blocked     => Superseded by (Reactor, Director),
        Blocked     => Abandoned  by (Reactor, Director),
        InReview    => InProgress by (Reactor),
        InReview    => Integrated by (Integrator),
        InReview    => Superseded by (Reactor, Director),
        InReview    => Abandoned  by (Reactor, Director),
        Integrated  => Done       by (Reactor),
        Integrated  => Superseded by (Reactor, Director),
        Integrated  => Abandoned  by (Reactor, Director),
    ),
    overrides(
        Ready      => Done     by (Reactor),
        InProgress => Ready    by (Reactor),
        InProgress => InReview by (Reactor),
        InReview   => Ready    by (Reactor),
        InReview   => Blocked  by (Reactor),
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
/// for Stage 7's Reactor (which scans "child Works of
/// this Plan" on every tick). `attempt_count`, `session_failure_count`,
/// `files`, and `assignee` ship with `Default` values; Stage 7
/// populates them when the Reactor and reviewer wire up. No
/// `description` field (v0.1.96 removed it). `blocked_reason` ships
/// here per docs/design/2026-05-07-dependency-gate.md Phase 1
/// (resolves scope memo D3 deferral).
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
    /// Set when a dep Work reaches a terminal non-Done state, explaining
    /// why this Work was blocked by the dep gate rather than by an agent.
    /// Written by `block_dependent_siblings` in `loopr`; read by 1.3's
    /// recovery loop and by the Work summary renderer.
    #[serde(default)]
    pub blocked_reason: Option<String>,
}

impl Work {
    /// New Work under the given Plan. Status starts `Pending` (not
    /// `Draft`) because the reactive-execution convention is that a
    /// freshly decomposed Work is immediately eligible for the
    /// Reactor's `Pending -> Ready` transition once its deps
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
            blocked_reason: None,
        }
    }

    /// True when every dep id in `self.dependencies` appears in
    /// `siblings` with status `Done`. Unknown dep ids (not present in
    /// `siblings`) return `false` - treated as unsatisfied. A Work with
    /// no dependencies always returns `true`.
    pub fn all_deps_done(&self, siblings: &[Work]) -> bool {
        self.dependencies
            .iter()
            .all(|dep_id| siblings.iter().any(|s| &s.id == dep_id && s.status == WorkStatus::Done))
    }

    /// Returns the first dep id whose Work appears in `siblings` with
    /// an irrecoverable (truly terminal, non-Done) status: `Abandoned`
    /// or `Superseded`. `Blocked` is excluded because it may recover
    /// via 1.3's recovery loop. Returns `None` when no dep is
    /// irrecoverable.
    pub fn any_dep_irrecoverable<'a>(&self, siblings: &'a [Work]) -> Option<&'a WorkId> {
        self.dependencies.iter().find_map(|dep_id| {
            siblings.iter().find_map(|s| {
                if &s.id == dep_id && matches!(s.status, WorkStatus::Abandoned | WorkStatus::Superseded) {
                    Some(&s.id)
                } else {
                    None
                }
            })
        })
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
