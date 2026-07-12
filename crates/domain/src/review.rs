//! `Review` record type: the persisted evidence of one review round for a
//! Bundle, plus the per-criterion leaf types its `criteria` field carries.
//!
//! Phase 7 of `docs/design/2026-07-11-verified-swarm.md` introduces the
//! record and its `ReviewsStore`; Phase 11 (persist reviews + deterministic
//! accept gate) is the first writer, appending one `Review` per round. This
//! phase is purely additive: the record, its typed id, and the store handle
//! land with zero consumers.
//!
//! No FSM: reviews are append-only history. Each round appends a new row
//! (`round = prior review count for the bundle + 1`); rows are never
//! mutated, so a crash between review-persist and Bundle-transition simply
//! re-reviews and appends another round. The record's `verdict` field
//! REUSES the existing `domain::Verdict` enum rather than minting a second
//! outcome vocabulary — the panel flagged the name collision, so the truth
//! lives in one type.

use serde::{Deserialize, Serialize};

use derive::Record;

use crate::id::{BundleId, CheckRunId, ReviewId, now_millis};
use crate::{ReviewIssue, Verdict};

/// Per-criterion outcome for one acceptance criterion in a review round.
///
/// The `criterion_id` references the criterion's stable id (minted by the
/// decomposer in Phase 8, when `AcceptanceCriteria` becomes `Vec<Criterion>`;
/// until then no criterion ids exist and no `CriterionResult` is written —
/// see `Review.criteria`). `evidence` optionally cites the check output or
/// diff hunk that justifies the status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionResult {
    pub criterion_id: u32,
    pub status: CriterionStatus,
    #[serde(default)]
    pub evidence: Option<String>,
}

/// Whether an acceptance criterion is satisfied. `Waived` is deliberately
/// absent: nothing writes it yet, and per the Data Model a status earns its
/// place only once a writer produces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CriterionStatus {
    Unmet,
    Met,
}

/// One review round's persisted evidence. Stored at
/// `<target>/.loopr/taskstore/reviews.jsonl` via the `Record` derive (the
/// on-disk filename is the struct ident lowercased and pluralized).
///
/// A `Review` denormalizes the round's outcome (`verdict` + `summary` +
/// structured `reasons`) alongside the `check_run_ids` it weighed and the
/// concrete `model` that produced it, so downstream routing and audit never
/// reconstruct structure from a rendered string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Record)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub id: ReviewId,
    /// Foreign key: which Bundle this round reviewed. Indexed so
    /// `ReviewsStore::list_by_bundle` is an SQLite index lookup rather
    /// than a full-table scan.
    #[record(indexed)]
    pub bundle_id: BundleId,
    pub updated_at: i64,
    pub created_at: i64,
    /// 1-based review round for this Bundle (`prior review count + 1`).
    pub round: u32,
    /// Reviewer outcome — REUSES `domain::Verdict` (`Accept` /
    /// `ChangeRequested` / `Reject`); no second verdict enum exists.
    pub verdict: Verdict,
    /// One-line human summary of the round (mirrors the verdict's own
    /// summary; persisted here for list rendering without deconstructing
    /// the tagged enum).
    pub summary: String,
    /// Structured per-issue detail for a `ChangeRequested` round; empty
    /// for a clean `Accept`.
    #[serde(default)]
    pub reasons: Vec<ReviewIssue>,
    /// Per-criterion status for this round. **Present-but-unwritten until
    /// Phase 8.** The type is wired now so the record shape is stable, but
    /// nothing constructs a `CriterionResult` this phase: Phase 8 turns
    /// `AcceptanceCriteria` into `Vec<Criterion>` and mints the ids these
    /// results reference, and Phase 11 wires the Reviewer to populate this
    /// field. Until then every persisted `Review` carries an empty `Vec`.
    #[serde(default)]
    pub criteria: Vec<CriterionResult>,
    /// The `CheckRun` records this round weighed (Phase 10 persists the
    /// CheckRuns; Phase 11 links them here). Empty until then.
    #[serde(default)]
    pub check_run_ids: Vec<CheckRunId>,
    /// Concrete model id the Reviewer's LLM call actually ran (the model
    /// the provider echoed), for pinning-discipline audit.
    pub model: String,
}

impl Review {
    /// Construct a fresh `Review`: fresh `ReviewId`, `created_at ==
    /// updated_at == now`, and an **empty `criteria` Vec** (Phase 8 defines
    /// the writers; see the field docstring). Phase 11's `run_reviewer` is
    /// the sole production caller; tests build records via this seam or
    /// directly.
    pub fn new(
        bundle_id: BundleId,
        round: u32,
        verdict: Verdict,
        summary: String,
        reasons: Vec<ReviewIssue>,
        check_run_ids: Vec<CheckRunId>,
        model: String,
    ) -> Self {
        let now = now_millis();
        Self {
            id: ReviewId::new(),
            bundle_id,
            updated_at: now,
            created_at: now,
            round,
            verdict,
            summary,
            reasons,
            criteria: Vec::new(),
            check_run_ids,
            model,
        }
    }
}

#[cfg(test)]
mod tests;
