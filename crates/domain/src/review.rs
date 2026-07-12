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
use crate::{CheckRun, ReviewIssue, Verdict};

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

// ---------------------------------------------------------------------------
// Deterministic accept gate (Phase 11)
// ---------------------------------------------------------------------------

/// Outcome of the deterministic accept-gate evidence check (Phase 11 of
/// `docs/design/2026-07-11-verified-swarm.md`).
///
/// Computed purely from the persisted `Review` history + `CheckRun` evidence
/// for a Bundle. The daemon's accept site (`spawner.rs`) and the Director
/// state summary (`agents::build_director_state`) both read it, so the gate
/// and the operator see the SAME verdict from one source of truth.
///
/// **Fail-closed:** only `Accept` permits `Reviewed -> Accepted`. Every other
/// variant refuses — missing evidence, an ambiguous (stale) round chain, a
/// non-accept latest verdict, or an accept that references red checks. The
/// prompt is not the gate; this decision is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptDecision {
    /// Latest `Review` is `Accept`, its `round` matches the append-only
    /// history length, and zero referenced `CheckRun`s are red -> accept
    /// permitted.
    Accept { round: u32 },
    /// No `Review` persisted for the Bundle -> refuse (evidence missing).
    NoReview,
    /// The latest `Review`'s `round` does not equal the number of persisted
    /// rounds (the append-only 1..N chain is broken / ambiguous) -> refuse
    /// (evidence stale). This is the "round mismatch" case.
    Stale { latest_round: u32, review_count: u32 },
    /// The latest `Review` is not an `Accept` -> refuse.
    NotAccept { verdict_kind: &'static str, round: u32 },
    /// The latest `Review` is `Accept` but references red (nonzero-exit)
    /// `CheckRun`s -> refuse. The Phase 10 code-gate should have prevented
    /// this at review time; refusing here is defense-in-depth.
    RedChecks { count: u32, round: u32 },
}

impl AcceptDecision {
    /// Only `Accept` permits the `Reviewed -> Accepted` transition.
    pub fn is_accept(&self) -> bool {
        matches!(self, AcceptDecision::Accept { .. })
    }

    /// Short human-readable evidence label. Rendered into the Director state
    /// summary per Reviewed bundle and into the accept-site warn log, so the
    /// operator and the gate log agree on why an accept was (or was not)
    /// permitted.
    pub fn evidence_label(&self) -> String {
        match self {
            AcceptDecision::Accept { round } => {
                format!("review round {round}: accept, 0 red checks (accept-eligible)")
            }
            AcceptDecision::NoReview => "NO REVIEW EVIDENCE on record -- accept refused by the gate".to_string(),
            AcceptDecision::Stale {
                latest_round,
                review_count,
            } => format!(
                "STALE REVIEW EVIDENCE (latest round {latest_round} != {review_count} rounds on record) -- accept refused"
            ),
            AcceptDecision::NotAccept { verdict_kind, round } => {
                format!("review round {round}: {verdict_kind} -- accept refused")
            }
            AcceptDecision::RedChecks { count, round } => {
                format!("review round {round}: accept over {count} red check(s) -- accept refused")
            }
        }
    }
}

/// Return the wire discriminator string for a `Verdict` kind (matches the
/// serde `rename_all = "snake_case"` tag). Used by the accept-gate and the
/// Director summary to name the latest verdict without deconstructing the
/// tagged enum at every call site.
pub fn verdict_kind(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Accept { .. } => "accept",
        Verdict::ChangeRequested { .. } => "change_requested",
        Verdict::Reject { .. } => "reject",
    }
}

/// Deterministic accept-gate decision for a Bundle, computed purely from its
/// persisted `Review` history and `CheckRun` evidence (Phase 11).
///
/// The latest round wins (`max` by `round`). The append-only history must be a
/// contiguous 1..N chain, so the latest round must equal the review count; a
/// mismatch is treated as ambiguous/stale evidence and refused. Only an
/// `Accept` latest verdict with zero red referenced CheckRuns permits the
/// accept. Everything else fails closed.
pub fn decide_accept(reviews: &[Review], check_runs: &[CheckRun]) -> AcceptDecision {
    if reviews.is_empty() {
        return AcceptDecision::NoReview;
    }
    let review_count = reviews.len() as u32;
    // Latest round wins. In a clean append-only history rounds are the unique
    // sequence 1..N, so the max round equals the count; the check below rejects
    // any history where that invariant is broken.
    let latest = reviews
        .iter()
        .max_by_key(|r| r.round)
        .expect("reviews is non-empty (checked above)");
    if latest.round != review_count {
        return AcceptDecision::Stale {
            latest_round: latest.round,
            review_count,
        };
    }
    let red = latest
        .check_run_ids
        .iter()
        .filter(|id| check_runs.iter().any(|c| c.id == **id && c.exit_code != 0))
        .count() as u32;
    match &latest.verdict {
        Verdict::Accept { .. } if red == 0 => AcceptDecision::Accept { round: latest.round },
        Verdict::Accept { .. } => AcceptDecision::RedChecks {
            count: red,
            round: latest.round,
        },
        other => AcceptDecision::NotAccept {
            verdict_kind: verdict_kind(other),
            round: latest.round,
        },
    }
}

#[cfg(test)]
mod tests;
