#![allow(clippy::unwrap_used)]
//! Unit tests for the `OperatorNote` record type (Phase 7 of
//! `docs/design/2026-05-09-director-phase-2.md`).

use crate::id::PlanId;

use super::OperatorNote;

#[test]
fn new_note_is_unread_and_has_matching_timestamps() {
    let plan_id = PlanId::new();
    let note = OperatorNote::new(plan_id.clone(), "alice".to_string(), "retry the build".to_string());
    assert_eq!(note.plan_id, plan_id);
    assert_eq!(note.author, "alice");
    assert_eq!(note.message, "retry the build");
    assert!(note.is_unread(), "fresh note must be unread");
    assert!(note.read_at.is_none());
    assert_eq!(
        note.created_at, note.updated_at,
        "fresh note must have created_at == updated_at"
    );
}

#[test]
fn mark_read_sets_read_at_and_advances_updated_at() {
    let plan_id = PlanId::new();
    let mut note = OperatorNote::new(plan_id, "bob".to_string(), "hi".to_string());
    let original_updated_at = note.updated_at;
    let ts = original_updated_at + 5000;
    note.mark_read(ts);
    assert_eq!(note.read_at, Some(ts), "mark_read must set the timestamp");
    assert_eq!(note.updated_at, ts, "mark_read must advance updated_at");
    assert!(!note.is_unread(), "post-mark_read note must not be unread");
}

#[test]
fn serde_round_trip_preserves_fields() {
    let plan_id = PlanId::new();
    let mut note = OperatorNote::new(plan_id, "carol".to_string(), "no big deal".to_string());
    note.mark_read(note.created_at + 100);
    let json = serde_json::to_string(&note).expect("serialize");
    let restored: OperatorNote = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.id, note.id);
    assert_eq!(restored.plan_id, note.plan_id);
    assert_eq!(restored.author, note.author);
    assert_eq!(restored.message, note.message);
    assert_eq!(restored.created_at, note.created_at);
    assert_eq!(restored.updated_at, note.updated_at);
    assert_eq!(restored.read_at, note.read_at);
}

#[test]
fn read_at_defaults_to_none_when_missing_in_json() {
    // Forward-compat: an older note file without the `read_at` field
    // (e.g. a write that predated the field's existence) must
    // deserialize cleanly with read_at = None.
    let plan_id = PlanId::new();
    let note = OperatorNote::new(plan_id, "dave".to_string(), "test".to_string());
    let mut value = serde_json::to_value(&note).expect("serialize");
    value.as_object_mut().unwrap().remove("read_at");
    let json = serde_json::to_string(&value).unwrap();
    let restored: OperatorNote = serde_json::from_str(&json).expect("deserialize without read_at");
    assert!(restored.read_at.is_none());
}
