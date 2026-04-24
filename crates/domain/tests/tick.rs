#![allow(clippy::unwrap_used)]

//! Seam tests for `Tick`. Exercises the crate's public API as an
//! external consumer. `Tick` has no FSM, so coverage is constructor,
//! serde posture, and `Record` trait impl.

use domain::{BundleId, PlanId, Tick};
use taskstore_traits::{IndexValue, Record};

// ---------------------------------------------------------------------------
// Tick::new
// ---------------------------------------------------------------------------

#[test]
fn tick_new_preserves_plan_id() {
    let pid = PlanId::new();
    let tick = Tick::new(
        pid.clone(),
        vec![BundleId::new()],
        "loopr/plan-xxx".to_string(),
        "abc123".to_string(),
        vec!["def456".to_string()],
    );
    assert_eq!(tick.plan_id, pid);
}

#[test]
fn tick_new_preserves_branch_and_sha_and_bundles() {
    let bid = BundleId::new();
    let tick = Tick::new(
        PlanId::new(),
        vec![bid.clone()],
        "loopr/plan-abc".to_string(),
        "deadbeef".to_string(),
        vec!["cafebabe".to_string()],
    );
    assert_eq!(tick.branch, "loopr/plan-abc");
    assert_eq!(tick.sha, "deadbeef");
    assert_eq!(tick.bundles, vec![bid]);
    assert_eq!(tick.merge_commits, vec!["cafebabe".to_string()]);
}

#[test]
fn tick_new_id_has_tk_prefix() {
    let tick = Tick::new(PlanId::new(), vec![], String::new(), String::new(), vec![]);
    assert!(
        tick.id.as_ref().starts_with("tk-"),
        "TickId must be tk-prefixed: {}",
        tick.id
    );
}

#[test]
fn tick_new_created_at_equals_updated_at() {
    let tick = Tick::new(PlanId::new(), vec![], String::new(), String::new(), vec![]);
    assert_eq!(tick.created_at, tick.updated_at);
}

#[test]
fn tick_new_distinct_calls_produce_distinct_ids() {
    let a = Tick::new(PlanId::new(), vec![], String::new(), String::new(), vec![]);
    let b = Tick::new(PlanId::new(), vec![], String::new(), String::new(), vec![]);
    assert_ne!(a.id, b.id);
}

#[test]
fn tick_new_allows_empty_bundles_and_commits() {
    // First-gate asserts bundles.len() == 1 at the Integrator layer, but the
    // Tick record itself does not enforce that; a caller could construct a
    // Tick with any Vec shape. This test pins the record-level contract.
    let tick = Tick::new(
        PlanId::new(),
        vec![],
        "loopr/plan-x".to_string(),
        "sha".to_string(),
        vec![],
    );
    assert!(tick.bundles.is_empty());
    assert!(tick.merge_commits.is_empty());
}

#[test]
fn tick_new_supports_multiple_bundles_and_commits() {
    // Multi-Bundle is forward-compat in the record; the Integrator's
    // allow_multi_bundle guard gates the agent-level decision, not the
    // record's construction.
    let b1 = BundleId::new();
    let b2 = BundleId::new();
    let tick = Tick::new(
        PlanId::new(),
        vec![b1.clone(), b2.clone()],
        "loopr/plan-x".to_string(),
        "sha3".to_string(),
        vec!["sha1".to_string(), "sha2".to_string()],
    );
    assert_eq!(tick.bundles, vec![b1, b2]);
    assert_eq!(tick.merge_commits.len(), 2);
}

// ---------------------------------------------------------------------------
// Tick serde
// ---------------------------------------------------------------------------

#[test]
fn tick_serde_roundtrip_json() {
    let tick = Tick::new(
        PlanId::new(),
        vec![BundleId::new()],
        "loopr/plan-abc".to_string(),
        "deadbeef".to_string(),
        vec!["cafebabe".to_string()],
    );
    let json = serde_json::to_string(&tick).unwrap();
    let back: Tick = serde_json::from_str(&json).unwrap();
    assert_eq!(tick.id, back.id);
    assert_eq!(tick.plan_id, back.plan_id);
    assert_eq!(tick.branch, back.branch);
    assert_eq!(tick.sha, back.sha);
    assert_eq!(tick.bundles, back.bundles);
    assert_eq!(tick.merge_commits, back.merge_commits);
    assert_eq!(tick.created_at, back.created_at);
    assert_eq!(tick.updated_at, back.updated_at);
}

#[test]
fn tick_serde_rejects_unknown_fields() {
    let bogus = r#"{
        "id": "tk-abc12",
        "plan_id": "pl-xyz34",
        "updated_at": 1700000000000,
        "created_at": 1700000000000,
        "branch": "loopr/plan-xyz34",
        "sha": "abc",
        "bundles": [],
        "merge_commits": [],
        "bogus_field": "fail"
    }"#;
    let result: Result<Tick, _> = serde_json::from_str(bogus);
    assert!(result.is_err(), "deny_unknown_fields must reject extra keys");
}

// ---------------------------------------------------------------------------
// Record trait impl
// ---------------------------------------------------------------------------

#[test]
fn tick_record_id_matches_as_ref() {
    let tick = Tick::new(PlanId::new(), vec![], String::new(), String::new(), vec![]);
    assert_eq!(<Tick as Record>::id(&tick), tick.id.as_ref());
}

#[test]
fn tick_record_updated_at_matches_field() {
    let tick = Tick::new(PlanId::new(), vec![], String::new(), String::new(), vec![]);
    assert_eq!(<Tick as Record>::updated_at(&tick), tick.updated_at);
}

#[test]
fn tick_record_collection_name() {
    assert_eq!(Tick::collection_name(), "ticks");
}

#[test]
fn tick_record_indexed_fields_one_entry() {
    let pid = PlanId::new();
    let tick = Tick::new(pid.clone(), vec![], String::new(), String::new(), vec![]);
    let fields = tick.indexed_fields();
    assert_eq!(fields.len(), 1, "exactly one indexed field expected (plan_id)");
    assert_eq!(
        fields.get("plan_id"),
        Some(&IndexValue::String(pid.as_ref().to_string())),
        "indexed plan_id must match PlanId wire form"
    );
}
