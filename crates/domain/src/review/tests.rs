#![allow(clippy::unwrap_used)]
//! Unit tests for the `Review` record type and its per-criterion leaf
//! types (Phase 7 of `docs/design/2026-07-11-verified-swarm.md`).

use crate::Verdict;
use crate::id::{BundleId, CheckRunId};

use super::{CriterionResult, CriterionStatus, Review};

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
