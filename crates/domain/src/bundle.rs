//! `Bundle` record type and `BundleStatus` state machine.
//!
//! A `Bundle` is the Implementer's output: a git commit range on a
//! Work's branch that the Reviewer evaluates and the Integrator merges.
//! Stage 7 introduces the type; downstream stages drive the FSM through
//! Proposed -> Triaged -> Reviewed -> Accepted -> Integrating ->
//! Merged as review and integration run.
//!
//! Fields ported from v3's `Bundle` post-v0.1.96 (description-field
//! removal) with v5 upgrades: typed `BundleId`/`WorkId`, indexed
//! `work_id` for `list_by_work_id`, `IntegrationFailed` as a distinct
//! terminal from `Rejected`, and no `description` field (v0.1.96
//! removed it; content lives in `docs/loopr/<id>.md`).

use serde::{Deserialize, Serialize};
use strum::Display;

use derive::{Fsm, Record};

use crate::id::{BundleId, WorkId, now_millis};
use crate::{FsmError, Role, Transition};

/// Lifecycle state for `Bundle`. `IntegrationFailed` is a distinct
/// terminal from `Rejected`: a merit-based rejection (Reviewer says
/// "this is wrong") is semantically different from an integration
/// failure (merge conflict, post-merge test break), and downstream
/// consumers branch on it without parsing a verification string.
///
/// `Reviewer` cannot act on `Proposed`: the Reactor always triages
/// first. `Proposed => Rejected` is Reactor-only.
///
/// Display output is lowercase to match the serde wire form - the
/// Record derive calls `ToString::to_string` on indexed fields, so
/// the index map value and the on-disk JSON value must use the same
/// spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, Fsm)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[fsm(
    role = crate::Role,
    terminal = [Merged, Rejected, IntegrationFailed, Superseded],
    transitions(
        Proposed    => Triaged           by (Reactor),
        Proposed    => Rejected          by (Reactor),
        Proposed    => Superseded        by (Reactor),
        Triaged     => Reviewed          by (Reactor, Reviewer),
        Triaged     => Accepted          by (Reactor),
        Triaged     => Rejected          by (Reactor, Reviewer),
        Triaged     => Superseded        by (Reactor),
        Reviewed    => Accepted          by (Reactor, Director),
        Reviewed    => Rejected          by (Reactor, Reviewer),
        Reviewed    => Superseded        by (Reactor),
        Accepted    => Integrating       by (Integrator),
        Accepted    => Superseded        by (Reactor),
        Integrating => Merged            by (Integrator),
        Integrating => IntegrationFailed by (Integrator),
        Integrating => Superseded        by (Reactor),
    ),
)]
pub enum BundleStatus {
    Proposed,
    Triaged,
    Reviewed,
    Accepted,
    Integrating,
    Merged,
    Rejected,
    IntegrationFailed,
    Superseded,
}

/// Implementer output record. Persisted at
/// `<target>/.loopr/taskstore/bundles.jsonl` via the `Record` derive.
///
/// Ported from v3's `Bundle` with v5 upgrades:
/// - Typed `BundleId` / `WorkId` instead of `String`
/// - `#[record(indexed)]` on `work_id` for efficient
///   `list_by_work_id` lookups via taskstore's SQLite index
/// - No `description` field (v0.1.96 removed it); Bundle references
///   Work, it does not carry spec content
/// - `force_proposed: bool` set by the Implementer only on the
///   iteration-cap fallback path; Reviewer policy keys on it
#[derive(Debug, Clone, Serialize, Deserialize, Record)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub id: BundleId,
    #[record(indexed)]
    pub work_id: WorkId,
    pub updated_at: i64,
    pub created_at: i64,
    pub branch_name: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub claims: Vec<String>,
    #[serde(default)]
    pub verification: String,
    #[serde(default)]
    pub loc_changed: Option<u32>,
    #[serde(default)]
    pub noop_reason: Option<String>,
    #[serde(default)]
    pub head_commit: Option<String>,
    /// Base sha the implementer branched from (the worktree's base at
    /// propose time). The Reviewer diffs `base_commit..head_commit` so a
    /// multi-commit bundle is reviewed across ALL its commits, not just
    /// the final one. `None` for noop (`Done`) bundles and pre-existing
    /// rows; the Reviewer falls back to `git show <head_commit>` then.
    #[serde(default)]
    pub base_commit: Option<String>,
    #[serde(default)]
    pub force_proposed: bool,
    #[record(indexed)]
    pub status: BundleStatus,
}

impl Bundle {
    /// New Bundle: fresh BundleId, status = Proposed, created_at =
    /// updated_at = now.
    ///
    /// The Implementer calls this at ProposeBundle time; Reviewer and
    /// Integrator transition the status via `transition`/`override_status`.
    pub fn new(work_id: WorkId, branch_name: String, claims: Vec<String>) -> Self {
        let now = now_millis();
        Self {
            id: BundleId::new(),
            work_id,
            updated_at: now,
            created_at: now,
            branch_name,
            paths: Vec::new(),
            claims,
            verification: String::new(),
            loc_changed: None,
            noop_reason: None,
            head_commit: None,
            base_commit: None,
            force_proposed: false,
            status: BundleStatus::Proposed,
        }
    }

    /// Read current status. The field is `pub`; this method exists
    /// for method-chain call sites.
    pub fn status(&self) -> BundleStatus {
        self.status
    }

    /// Validated FSM transition. Delegates to the Fsm-derived
    /// `validate_transition`. On any state-changing result
    /// (`Changed`), updates `self.status` and `self.updated_at`.
    /// `Unchanged` (from == to) leaves state intact. Invalid
    /// transitions return `FsmError`.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(record_kind = "bundle", record_id = %self.id, from = ?self.status, target = ?target, role = ?role),
        ret,
        err,
    )]
    pub fn transition(&mut self, target: BundleStatus, role: Role) -> Result<Transition, FsmError<BundleStatus>> {
        let result = BundleStatus::validate_transition(self.status, target, role)?;
        if result != Transition::Unchanged {
            self.status = target;
            self.updated_at = now_millis();
        }
        Ok(result)
    }

    /// Validated FSM override. Delegates to the Fsm-derived
    /// `validate_override`. Any state-changing result (`Changed` or
    /// `Override`) updates `self.status` and `self.updated_at`; only
    /// `Unchanged` leaves state intact.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(record_kind = "bundle", record_id = %self.id, from = ?self.status, target = ?target, role = ?role, override_ = true),
        ret,
        err,
    )]
    pub fn override_status(&mut self, target: BundleStatus, role: Role) -> Result<Transition, FsmError<BundleStatus>> {
        let result = BundleStatus::validate_override(self.status, target, role)?;
        if result != Transition::Unchanged {
            self.status = target;
            self.updated_at = now_millis();
        }
        Ok(result)
    }
}
