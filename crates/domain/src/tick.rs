//! `Tick` record type.
//!
//! A `Tick` is the Integrator's output: a record of one or more
//! Accepted Bundles having been merged into a Plan's integration
//! branch (`loopr/plan-<plan-id>`). Every Tick is born in its final
//! state (the merge has already landed when the Tick is persisted);
//! it has no FSM.
//!
//! First gate: one Bundle per Tick. `bundles` and `merge_commits`
//! have a stable 1:1 index relationship - `bundles[i]` was merged at
//! `merge_commits[i]`. The slice-based `Tick::new` signature keeps
//! multi-Bundle forward-compatible.

use serde::{Deserialize, Serialize};

use derive::Record;

use crate::id::{BundleId, PlanId, TickId, now_millis};

/// Integrator output record. Persisted at
/// `<target>/.loopr/taskstore/ticks.jsonl` via the `Record` derive.
///
/// The `plan_id` field is indexed for `TicksStore::list_by_plan_id`.
/// The `branch` string duplicates information derivable
/// from `plan_id` (it is always `loopr/plan-<plan_id>`) but is
/// carried on the record to save a `PlansStore` join for audit
/// queries and to future-proof against branch-naming changes.
#[derive(Debug, Clone, Serialize, Deserialize, Record)]
#[serde(deny_unknown_fields)]
pub struct Tick {
    pub id: TickId,
    #[record(indexed)]
    pub plan_id: PlanId,
    pub updated_at: i64,
    pub created_at: i64,
    pub branch: String,
    pub sha: String,
    pub bundles: Vec<BundleId>,
    pub merge_commits: Vec<String>,
}

/// Construction error for `Tick::new`.
///
/// Hand-rolled `Display` + `std::error::Error` to match `domain`'s
/// existing `FsmError`/`GraphError` style (`crate::fsm`, `crate::graph`);
/// `domain` has no `thiserror` dependency and this type does not add one.
#[derive(Debug)]
pub enum TickError {
    /// The 1:1 index relationship between `bundles` and `merge_commits`
    /// (`bundles[i]` was merged at `merge_commits[i]`) does not hold.
    /// Previously a `debug_assert_eq!` (a release-build no-op panic
    /// trap); promoted to a typed, always-checked error so a wiring bug
    /// surfaces as a handleable `Result`, not a debug-only panic.
    LengthMismatch { bundles: usize, merge_commits: usize },
}

impl std::fmt::Display for TickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TickError::LengthMismatch { bundles, merge_commits } => write!(
                f,
                "Tick::new: bundles/merge_commits length mismatch ({bundles} != {merge_commits})"
            ),
        }
    }
}

impl std::error::Error for TickError {}

impl Tick {
    /// New Tick: fresh TickId, created_at = updated_at = now.
    ///
    /// The Integrator constructs a Tick at the top of its Phase 3
    /// commit path with the merge outcomes produced by Phase 2. The
    /// Integrator is responsible for ensuring `bundles.len() ==
    /// merge_commits.len()` and that the 1:1 index order is
    /// preserved; this constructor enforces it as a typed `Result`
    /// rather than trusting the caller.
    pub fn new(
        plan_id: PlanId,
        bundles: Vec<BundleId>,
        branch: String,
        sha: String,
        merge_commits: Vec<String>,
    ) -> Result<Self, TickError> {
        // The 1:1 index relationship between `bundles` and
        // `merge_commits` is the Integrator's documented contract
        // (`bundles[i]` was merged at `merge_commits[i]`). Checked in
        // every build (not just debug) so a wiring bug surfaces as a
        // typed error at construction rather than a silent off-by-one
        // in audit queries or a debug-only panic.
        if bundles.len() != merge_commits.len() {
            return Err(TickError::LengthMismatch {
                bundles: bundles.len(),
                merge_commits: merge_commits.len(),
            });
        }
        let now = now_millis();
        Ok(Self {
            id: TickId::new(),
            plan_id,
            updated_at: now,
            created_at: now,
            branch,
            sha,
            bundles,
            merge_commits,
        })
    }
}
