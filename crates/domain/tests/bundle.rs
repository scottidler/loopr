#![allow(clippy::unwrap_used)]

//! Seam tests for `Bundle` + `BundleStatus`. Exercises the crate's
//! public API as an external consumer.

use domain::{Bundle, BundleStatus, FsmErrorKind, Role, Transition, WorkId};
use taskstore_traits::{IndexValue, Record};

// ---------------------------------------------------------------------------
// Bundle::new
// ---------------------------------------------------------------------------

#[test]
fn bundle_new_defaults_to_proposed() {
    let bundle = Bundle::new(WorkId::new(), "wk-abc12/1".to_string(), vec!["claim".to_string()]);
    assert_eq!(bundle.status(), BundleStatus::Proposed);
}

#[test]
fn bundle_new_preserves_work_id() {
    let wid = WorkId::new();
    let bundle = Bundle::new(wid.clone(), "b".to_string(), vec![]);
    assert_eq!(bundle.work_id, wid);
}

#[test]
fn bundle_new_preserves_branch_and_claims() {
    let bundle = Bundle::new(
        WorkId::new(),
        "my-branch".to_string(),
        vec!["claim1".to_string(), "claim2".to_string()],
    );
    assert_eq!(bundle.branch_name, "my-branch");
    assert_eq!(bundle.claims, vec!["claim1".to_string(), "claim2".to_string()]);
}

#[test]
fn bundle_new_id_has_bd_prefix() {
    let bundle = Bundle::new(WorkId::new(), "b".to_string(), vec![]);
    assert!(
        bundle.id.as_ref().starts_with("bd-"),
        "BundleId must be bd-prefixed: {}",
        bundle.id
    );
}

#[test]
fn bundle_new_created_at_equals_updated_at() {
    let bundle = Bundle::new(WorkId::new(), "b".to_string(), vec![]);
    assert_eq!(bundle.created_at, bundle.updated_at);
}

#[test]
fn bundle_new_optional_fields_default() {
    let bundle = Bundle::new(WorkId::new(), "b".to_string(), vec![]);
    assert!(bundle.paths.is_empty());
    assert_eq!(bundle.verification, "");
    assert_eq!(bundle.loc_changed, None);
    assert_eq!(bundle.noop_reason, None);
    assert_eq!(bundle.head_commit, None);
    assert!(!bundle.force_proposed);
}

#[test]
fn bundle_new_distinct_calls_produce_distinct_ids() {
    let a = Bundle::new(WorkId::new(), "a".to_string(), vec![]);
    let b = Bundle::new(WorkId::new(), "b".to_string(), vec![]);
    assert_ne!(a.id, b.id);
}

// ---------------------------------------------------------------------------
// Bundle serde
// ---------------------------------------------------------------------------

#[test]
fn bundle_serde_roundtrip_json() {
    let bundle = Bundle::new(WorkId::new(), "branch".to_string(), vec!["c".to_string()]);
    let json = serde_json::to_string(&bundle).unwrap();
    let back: Bundle = serde_json::from_str(&json).unwrap();
    assert_eq!(bundle.id, back.id);
    assert_eq!(bundle.work_id, back.work_id);
    assert_eq!(bundle.branch_name, back.branch_name);
    assert_eq!(bundle.claims, back.claims);
    assert_eq!(bundle.status, back.status);
    assert_eq!(bundle.created_at, back.created_at);
    assert_eq!(bundle.updated_at, back.updated_at);
    assert_eq!(bundle.force_proposed, back.force_proposed);
}

#[test]
fn bundle_serde_rejects_unknown_fields() {
    let bogus = r#"{
        "id": "bd-abc12",
        "work_id": "wk-xyz34",
        "updated_at": 1700000000000,
        "created_at": 1700000000000,
        "branch_name": "b",
        "status": "proposed",
        "bogus_field": "fail"
    }"#;
    let result: Result<Bundle, _> = serde_json::from_str(bogus);
    assert!(result.is_err(), "deny_unknown_fields must reject extra keys");
}

#[test]
fn bundle_serde_accepts_minimal_json() {
    let minimal = r#"{
        "id": "bd-abc12",
        "work_id": "wk-xyz34",
        "updated_at": 1700000000000,
        "created_at": 1700000000000,
        "branch_name": "b",
        "status": "proposed"
    }"#;
    let b: Bundle = serde_json::from_str(minimal).unwrap();
    assert!(b.paths.is_empty());
    assert!(b.claims.is_empty());
    assert_eq!(b.verification, "");
    assert_eq!(b.loc_changed, None);
    assert_eq!(b.head_commit, None);
    assert!(!b.force_proposed);
}

#[test]
fn bundle_serde_status_wire_form_lowercase() {
    let bundle = Bundle::new(WorkId::new(), "b".to_string(), vec![]);
    let json = serde_json::to_string(&bundle).unwrap();
    assert!(
        json.contains("\"status\":\"proposed\""),
        "status wire form must be lowercase: {json}"
    );
}

#[test]
fn bundle_serde_unknown_status_rejects() {
    let bogus = r#"{
        "id": "bd-abc12",
        "work_id": "wk-xyz34",
        "updated_at": 1700000000000,
        "created_at": 1700000000000,
        "branch_name": "b",
        "status": "not-a-state"
    }"#;
    let result: Result<Bundle, _> = serde_json::from_str(bogus);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// BundleStatus Display
// ---------------------------------------------------------------------------

#[test]
fn bundle_status_display_lowercase() {
    assert_eq!(format!("{}", BundleStatus::Proposed), "proposed");
    assert_eq!(format!("{}", BundleStatus::Triaged), "triaged");
    assert_eq!(format!("{}", BundleStatus::Reviewed), "reviewed");
    assert_eq!(format!("{}", BundleStatus::Accepted), "accepted");
    assert_eq!(format!("{}", BundleStatus::Integrating), "integrating");
    assert_eq!(format!("{}", BundleStatus::Merged), "merged");
    assert_eq!(format!("{}", BundleStatus::Rejected), "rejected");
    assert_eq!(format!("{}", BundleStatus::IntegrationFailed), "integrationfailed");
    assert_eq!(format!("{}", BundleStatus::Superseded), "superseded");
}

// ---------------------------------------------------------------------------
// Record trait impl
// ---------------------------------------------------------------------------

#[test]
fn bundle_record_id_matches_as_ref() {
    let bundle = Bundle::new(WorkId::new(), "b".to_string(), vec![]);
    assert_eq!(<Bundle as Record>::id(&bundle), bundle.id.as_ref());
}

#[test]
fn bundle_record_updated_at_matches_field() {
    let bundle = Bundle::new(WorkId::new(), "b".to_string(), vec![]);
    assert_eq!(<Bundle as Record>::updated_at(&bundle), bundle.updated_at);
}

#[test]
fn bundle_record_collection_name() {
    assert_eq!(Bundle::collection_name(), "bundles");
}

#[test]
fn bundle_record_indexed_fields_two_entries() {
    let wid = WorkId::new();
    let bundle = Bundle::new(wid.clone(), "b".to_string(), vec![]);
    let fields = bundle.indexed_fields();
    assert_eq!(fields.len(), 2, "exactly two indexed fields expected");
    assert_eq!(
        fields.get("status"),
        Some(&IndexValue::String("proposed".to_string())),
        "indexed status must use lowercase Display output"
    );
    assert_eq!(
        fields.get("work_id"),
        Some(&IndexValue::String(wid.as_ref().to_string())),
        "indexed work_id must match WorkId wire form"
    );
}

// ---------------------------------------------------------------------------
// FSM transitions - happy path. One test per transition edge (15 total).
// ---------------------------------------------------------------------------

fn bundle_in(status: BundleStatus) -> Bundle {
    let mut b = Bundle::new(WorkId::new(), "b".to_string(), vec![]);
    b.status = status;
    b
}

fn assert_changed(b: &mut Bundle, to: BundleStatus, role: Role) {
    let before = b.updated_at;
    std::thread::sleep(std::time::Duration::from_millis(2));
    let result = b.transition(to, role).unwrap();
    assert_eq!(
        result,
        Transition::Changed,
        "expected Changed for {:?} -> {:?} by {:?}",
        b.status,
        to,
        role
    );
    assert_eq!(b.status, to, "status must mutate");
    assert!(b.updated_at > before, "updated_at must advance");
}

#[test]
fn transition_proposed_triaged_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Proposed),
        BundleStatus::Triaged,
        Role::Reactor,
    );
}
#[test]
fn transition_proposed_rejected_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Proposed),
        BundleStatus::Rejected,
        Role::Reactor,
    );
}
#[test]
fn transition_proposed_superseded_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Proposed),
        BundleStatus::Superseded,
        Role::Reactor,
    );
}
#[test]
fn transition_triaged_reviewed_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Triaged),
        BundleStatus::Reviewed,
        Role::Reactor,
    );
}
#[test]
fn transition_triaged_reviewed_by_reviewer() {
    assert_changed(
        &mut bundle_in(BundleStatus::Triaged),
        BundleStatus::Reviewed,
        Role::Reviewer,
    );
}
#[test]
fn transition_triaged_accepted_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Triaged),
        BundleStatus::Accepted,
        Role::Reactor,
    );
}
#[test]
fn transition_triaged_rejected_by_reviewer() {
    assert_changed(
        &mut bundle_in(BundleStatus::Triaged),
        BundleStatus::Rejected,
        Role::Reviewer,
    );
}
#[test]
fn transition_triaged_superseded_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Triaged),
        BundleStatus::Superseded,
        Role::Reactor,
    );
}
#[test]
fn transition_reviewed_accepted_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Reviewed),
        BundleStatus::Accepted,
        Role::Reactor,
    );
}
#[test]
fn transition_reviewed_rejected_by_reviewer() {
    assert_changed(
        &mut bundle_in(BundleStatus::Reviewed),
        BundleStatus::Rejected,
        Role::Reviewer,
    );
}
#[test]
fn transition_reviewed_superseded_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Reviewed),
        BundleStatus::Superseded,
        Role::Reactor,
    );
}
#[test]
fn transition_accepted_integrating_by_integrator() {
    assert_changed(
        &mut bundle_in(BundleStatus::Accepted),
        BundleStatus::Integrating,
        Role::Integrator,
    );
}
#[test]
fn transition_accepted_superseded_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Accepted),
        BundleStatus::Superseded,
        Role::Reactor,
    );
}
#[test]
fn transition_integrating_merged_by_integrator() {
    assert_changed(
        &mut bundle_in(BundleStatus::Integrating),
        BundleStatus::Merged,
        Role::Integrator,
    );
}
#[test]
fn transition_integrating_integrationfailed_by_integrator() {
    assert_changed(
        &mut bundle_in(BundleStatus::Integrating),
        BundleStatus::IntegrationFailed,
        Role::Integrator,
    );
}
#[test]
fn transition_integrating_superseded_by_reactor() {
    assert_changed(
        &mut bundle_in(BundleStatus::Integrating),
        BundleStatus::Superseded,
        Role::Reactor,
    );
}

// ---------------------------------------------------------------------------
// FSM rejections - dead/unauthorized paths
// ---------------------------------------------------------------------------

#[test]
fn proposed_to_rejected_by_reviewer_rejects() {
    // Reviewer cannot act on Proposed - Reactor always triages first.
    // This test pins the design decision that Proposed => Rejected is
    // Reactor-only.
    let mut b = bundle_in(BundleStatus::Proposed);
    let err = b.transition(BundleStatus::Rejected, Role::Reviewer).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::RoleNotAuthorized);
    assert_eq!(b.status, BundleStatus::Proposed);
}

#[test]
fn transition_no_edge_rejects() {
    // Proposed -> Accepted is not in the transitions table.
    let mut b = bundle_in(BundleStatus::Proposed);
    let err = b.transition(BundleStatus::Accepted, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
    assert_eq!(b.status, BundleStatus::Proposed);
}

#[test]
fn transition_wrong_role_rejects() {
    // Accepted -> Integrating is Integrator-only.
    let mut b = bundle_in(BundleStatus::Accepted);
    let err = b.transition(BundleStatus::Integrating, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::RoleNotAuthorized);
    assert_eq!(b.status, BundleStatus::Accepted);
}

#[test]
fn transition_from_terminal_merged_rejects() {
    let mut b = bundle_in(BundleStatus::Merged);
    let err = b.transition(BundleStatus::Integrating, Role::Integrator).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
}

#[test]
fn transition_from_terminal_rejected_rejects() {
    let mut b = bundle_in(BundleStatus::Rejected);
    let err = b.transition(BundleStatus::Triaged, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
}

#[test]
fn transition_from_terminal_integration_failed_rejects() {
    let mut b = bundle_in(BundleStatus::IntegrationFailed);
    let err = b.transition(BundleStatus::Merged, Role::Integrator).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
}

#[test]
fn transition_from_terminal_superseded_rejects() {
    let mut b = bundle_in(BundleStatus::Superseded);
    let err = b.transition(BundleStatus::Triaged, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
}

// ---------------------------------------------------------------------------
// Unchanged + is_terminal
// ---------------------------------------------------------------------------

#[test]
fn transition_same_state_is_unchanged() {
    let mut b = Bundle::new(WorkId::new(), "b".to_string(), vec![]);
    let before = b.updated_at;
    let result = b.transition(BundleStatus::Proposed, Role::Reactor).unwrap();
    assert_eq!(result, Transition::Unchanged);
    assert_eq!(b.status, BundleStatus::Proposed);
    assert_eq!(b.updated_at, before, "Unchanged must not advance updated_at");
}

#[test]
fn bundle_status_terminal_states() {
    assert!(BundleStatus::Merged.is_terminal());
    assert!(BundleStatus::Rejected.is_terminal());
    assert!(BundleStatus::IntegrationFailed.is_terminal());
    assert!(BundleStatus::Superseded.is_terminal());
    assert!(!BundleStatus::Proposed.is_terminal());
    assert!(!BundleStatus::Triaged.is_terminal());
    assert!(!BundleStatus::Reviewed.is_terminal());
    assert!(!BundleStatus::Accepted.is_terminal());
    assert!(!BundleStatus::Integrating.is_terminal());
}
