#![allow(clippy::unwrap_used)]
//! Unit tests for the `Review` record type and its per-criterion leaf
//! types (Phase 7 of `docs/design/2026-07-11-verified-swarm.md`).

use crate::id::{BundleId, CheckRunId, WorkId};
use crate::{CheckRun, ReviewIssue, Role, Severity, Verdict};

use super::{AcceptDecision, CriterionResult, CriterionStatus, Review, decide_accept};

fn sample() -> Review {
    Review::new(
        BundleId::new(),
        1,
        Verdict::Accept {
            summary: "looks good".to_string(),
        },
        "looks good".to_string(),
        Vec::new(),
        vec![CheckRunId::new()],
        "claude-opus-4-8".to_string(),
    )
}

#[test]
fn new_stamps_id_matching_timestamps_and_empty_criteria() {
    let rv = sample();
    assert!(rv.id.as_ref().starts_with("rv-"), "ReviewId uses the rv- prefix");
    assert_eq!(
        rv.created_at, rv.updated_at,
        "fresh Review must have created_at == updated_at"
    );
    assert_eq!(rv.round, 1);
    assert!(
        rv.criteria.is_empty(),
        "criteria is present-but-empty until Phase 8 defines its writers"
    );
    assert_eq!(rv.check_run_ids.len(), 1);
}

#[test]
fn serde_round_trip_preserves_fields() {
    let rv = sample();
    let json = serde_json::to_string(&rv).expect("serialize");
    let restored: Review = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, rv, "round-trip must preserve every field");
}

#[test]
fn serde_round_trip_with_populated_criteria() {
    // The `criteria` field is unwritten by production this phase, but the
    // type must round-trip so Phase 8's writers land on a stable shape.
    let mut rv = sample();
    rv.criteria = vec![
        CriterionResult {
            criterion_id: 0,
            status: CriterionStatus::Met,
            evidence: Some("cargo test green".to_string()),
        },
        CriterionResult {
            criterion_id: 1,
            status: CriterionStatus::Unmet,
            evidence: None,
        },
    ];
    let json = serde_json::to_string(&rv).expect("serialize");
    assert!(
        json.contains("\"status\":\"met\""),
        "CriterionStatus serializes lowercase: {json}"
    );
    let restored: Review = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, rv);
}

#[test]
fn unknown_field_is_rejected() {
    let rv = sample();
    let mut value = serde_json::to_value(&rv).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("bogus".to_string(), serde_json::json!(true));
    let json = serde_json::to_string(&value).unwrap();
    let err = serde_json::from_str::<Review>(&json).unwrap_err();
    assert!(
        err.to_string().contains("bogus"),
        "error names the unknown field: {err}"
    );
}

#[test]
fn additive_vec_fields_default_when_missing() {
    // Forward-compat: a minimal row lacking the additive Vec fields
    // deserializes them as empty rather than failing.
    let rv = sample();
    let mut value = serde_json::to_value(&rv).unwrap();
    let obj = value.as_object_mut().unwrap();
    obj.remove("reasons");
    obj.remove("criteria");
    obj.remove("check_run_ids");
    let json = serde_json::to_string(&value).unwrap();
    let restored: Review = serde_json::from_str(&json).expect("deserialize without additive vecs");
    assert!(restored.reasons.is_empty());
    assert!(restored.criteria.is_empty());
    assert!(restored.check_run_ids.is_empty());
}

// ---------------------------------------------------------------------------
// decide_accept (Phase 11 deterministic accept gate)
// ---------------------------------------------------------------------------

fn accept_review(bundle_id: BundleId, round: u32, check_run_ids: Vec<CheckRunId>) -> Review {
    Review::new(
        bundle_id,
        round,
        Verdict::Accept {
            summary: "ok".to_string(),
        },
        "ok".to_string(),
        Vec::new(),
        check_run_ids,
        "claude-opus-4-8".to_string(),
    )
}

fn change_review(bundle_id: BundleId, round: u32) -> Review {
    Review::new(
        bundle_id,
        round,
        Verdict::ChangeRequested {
            summary: "fix".to_string(),
            reasons: vec![ReviewIssue {
                severity: Severity::Error,
                file: "src.rs".to_string(),
                line: None,
                message: "boom".to_string(),
                suggestion: None,
            }],
        },
        "fix".to_string(),
        Vec::new(),
        Vec::new(),
        "claude-opus-4-8".to_string(),
    )
}

fn check_run(bundle_id: BundleId, exit_code: i32) -> CheckRun {
    CheckRun::new(
        bundle_id,
        WorkId::new(),
        "cargo test".to_string(),
        exit_code,
        "digest".to_string(),
        "excerpt".to_string(),
        Role::Reviewer,
        10,
    )
}

#[test]
fn decide_accept_no_reviews_is_no_review() {
    assert_eq!(decide_accept(&[], &[]), AcceptDecision::NoReview);
    let d = decide_accept(&[], &[]);
    assert!(!d.is_accept(), "missing evidence must never permit accept");
}

#[test]
fn decide_accept_clean_accept_zero_red_is_accept() {
    let b = BundleId::new();
    let reviews = vec![accept_review(b.clone(), 1, Vec::new())];
    let d = decide_accept(&reviews, &[]);
    assert_eq!(d, AcceptDecision::Accept { round: 1 });
    assert!(d.is_accept());
}

#[test]
fn decide_accept_latest_round_wins_change_requested_refused() {
    // Round 1 accepted, round 2 requested changes: the latest round wins and
    // the accept is refused.
    let b = BundleId::new();
    let reviews = vec![accept_review(b.clone(), 1, Vec::new()), change_review(b.clone(), 2)];
    let d = decide_accept(&reviews, &[]);
    assert_eq!(
        d,
        AcceptDecision::NotAccept {
            verdict_kind: "change_requested",
            round: 2,
        }
    );
    assert!(!d.is_accept());
}

#[test]
fn decide_accept_stale_round_mismatch_refused() {
    // A single review whose round claims 5 when only 1 round is on record:
    // the append-only chain is broken -> Stale -> refused. Break-to-prove: an
    // accept verdict does NOT rescue a mismatched round.
    let b = BundleId::new();
    let reviews = vec![accept_review(b.clone(), 5, Vec::new())];
    let d = decide_accept(&reviews, &[]);
    assert_eq!(
        d,
        AcceptDecision::Stale {
            latest_round: 5,
            review_count: 1,
        }
    );
    assert!(!d.is_accept());
}

#[test]
fn decide_accept_accept_over_red_referenced_check_refused() {
    // The latest Accept references a red CheckRun -> refused (defense in depth
    // behind the Phase 10 code gate).
    let b = BundleId::new();
    let red = check_run(b.clone(), 1);
    let reviews = vec![accept_review(b.clone(), 1, vec![red.id.clone()])];
    let d = decide_accept(&reviews, &[red]);
    assert_eq!(d, AcceptDecision::RedChecks { count: 1, round: 1 });
    assert!(!d.is_accept());
}

#[test]
fn decide_accept_accept_over_green_referenced_check_accepts() {
    let b = BundleId::new();
    let green = check_run(b.clone(), 0);
    let reviews = vec![accept_review(b.clone(), 1, vec![green.id.clone()])];
    let d = decide_accept(&reviews, &[green]);
    assert_eq!(d, AcceptDecision::Accept { round: 1 });
    assert!(d.is_accept());
}
