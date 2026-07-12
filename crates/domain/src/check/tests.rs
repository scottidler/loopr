#![allow(clippy::unwrap_used)]
//! Unit tests for the `CheckRun` record type (Phase 7 of
//! `docs/design/2026-07-11-verified-swarm.md`).

use crate::Role;
use crate::id::{BundleId, WorkId};

use super::CheckRun;

fn sample() -> CheckRun {
    CheckRun::new(
        BundleId::new(),
        WorkId::new(),
        "cargo test --workspace".to_string(),
        0,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
        "test result: ok. 42 passed; 0 failed".to_string(),
        Role::Reviewer,
        1234,
    )
}

#[test]
fn new_stamps_id_and_matching_timestamps() {
    let cr = sample();
    assert!(cr.id.as_ref().starts_with("cr-"), "CheckRunId uses the cr- prefix");
    assert_eq!(
        cr.created_at, cr.updated_at,
        "fresh CheckRun must have created_at == updated_at"
    );
    assert_eq!(cr.executor, Role::Reviewer);
    assert!(cr.passed(), "exit_code 0 is a pass");
}

#[test]
fn passed_is_false_for_nonzero_exit() {
    let mut cr = sample();
    cr.exit_code = 101;
    assert!(!cr.passed(), "nonzero exit_code is a failure");
}

#[test]
fn serde_round_trip_preserves_fields() {
    let cr = sample();
    let json = serde_json::to_string(&cr).expect("serialize");
    let restored: CheckRun = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, cr, "round-trip must preserve every field");
    // executor's wire form is the kebab/lowercase Role spelling.
    assert!(
        json.contains("\"executor\":\"reviewer\""),
        "role serializes lowercase: {json}"
    );
}

#[test]
fn unknown_field_is_rejected() {
    // `deny_unknown_fields`: a typo or a field from a newer schema is a
    // loud error, not silent data loss.
    let cr = sample();
    let mut value = serde_json::to_value(&cr).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("bogus".to_string(), serde_json::json!(1));
    let json = serde_json::to_string(&value).unwrap();
    let err = serde_json::from_str::<CheckRun>(&json).unwrap_err();
    assert!(
        err.to_string().contains("bogus"),
        "error names the unknown field: {err}"
    );
}
