#![allow(clippy::unwrap_used)]

//! Seam tests for `Plan` + `PlanStatus`. Exercises the crate's public API
//! as an external consumer (integration tests compile as a separate crate).

use domain::{FsmErrorKind, Plan, PlanStatus, Role, Transition};
use taskstore_traits::{IndexValue, Record};

// ---------------------------------------------------------------------------
// Construction & serde
// ---------------------------------------------------------------------------

#[test]
fn plan_new_defaults() {
    let plan = Plan::new("Add --version flag".to_string());
    assert!(
        plan.id.as_ref().starts_with("pl-"),
        "id should be pl-prefixed: {}",
        plan.id
    );
    assert_eq!(plan.status(), PlanStatus::Active);
    assert_eq!(plan.status, plan.status());
    assert_eq!(plan.created_at, plan.updated_at);
    assert_eq!(plan.goal, "Add --version flag");
}

#[test]
fn plan_serde_roundtrip_json() {
    let plan = Plan::new("A goal".to_string());
    let json = serde_json::to_string(&plan).unwrap();
    let back: Plan = serde_json::from_str(&json).unwrap();
    assert_eq!(plan.id, back.id);
    assert_eq!(plan.goal, back.goal);
    assert_eq!(plan.status, back.status);
    assert_eq!(plan.created_at, back.created_at);
    assert_eq!(plan.updated_at, back.updated_at);
}

#[test]
fn plan_serde_rejects_unknown_fields() {
    // #[serde(deny_unknown_fields)] should reject a record with extra keys.
    let bogus = r#"{
        "id": "pl-k7m2p",
        "updated_at": 1700000000000,
        "created_at": 1700000000000,
        "goal": "test",
        "status": "active",
        "bogus_field": "should fail"
    }"#;
    let result: Result<Plan, _> = serde_json::from_str(bogus);
    assert!(result.is_err(), "deny_unknown_fields must reject extra keys");
}

#[test]
fn plan_serde_status_wire_form_lowercase() {
    // Pins #[serde(rename_all = "lowercase")] on PlanStatus.
    let plan = Plan::new("g".to_string());
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        json.contains("\"status\":\"active\""),
        "status wire form must be lowercase: {json}"
    );
}

// ---------------------------------------------------------------------------
// PlanStatus Display <-> serde alignment
// ---------------------------------------------------------------------------

#[test]
fn plan_status_display_lowercase() {
    // Display output (used by #[derive(Record)] to populate the index map)
    // must match the serde wire form. Divergence here silently breaks any
    // future query layer that matches index against raw JSON.
    assert_eq!(format!("{}", PlanStatus::Draft), "draft");
    assert_eq!(format!("{}", PlanStatus::Pending), "pending");
    assert_eq!(format!("{}", PlanStatus::Active), "active");
    assert_eq!(format!("{}", PlanStatus::Complete), "complete");
    assert_eq!(format!("{}", PlanStatus::Superseded), "superseded");
    assert_eq!(format!("{}", PlanStatus::Abandoned), "abandoned");
}

// ---------------------------------------------------------------------------
// Record trait impl
// ---------------------------------------------------------------------------

#[test]
fn plan_record_id_matches_as_ref() {
    let plan = Plan::new("g".to_string());
    assert_eq!(<Plan as Record>::id(&plan), plan.id.as_ref());
}

#[test]
fn plan_record_updated_at_matches_field() {
    let plan = Plan::new("g".to_string());
    assert_eq!(<Plan as Record>::updated_at(&plan), plan.updated_at);
}

#[test]
fn plan_record_collection_name() {
    assert_eq!(Plan::collection_name(), "plans");
}

#[test]
fn plan_record_indexed_fields_single_entry() {
    let plan = Plan::new("g".to_string());
    let fields = plan.indexed_fields();
    assert_eq!(fields.len(), 1, "exactly one indexed field expected");
    assert_eq!(
        fields.get("status"),
        Some(&IndexValue::String("active".to_string())),
        "indexed status must use lowercase Display output"
    );
}

// ---------------------------------------------------------------------------
// FSM transitions (normal table)
// ---------------------------------------------------------------------------

#[test]
fn plan_transition_active_complete_by_decomposer() {
    let mut plan = Plan::new("g".to_string());
    let before = plan.updated_at;
    // Sleep one ms to guarantee updated_at advances.
    std::thread::sleep(std::time::Duration::from_millis(2));
    let result = plan.transition(PlanStatus::Complete, Role::Decomposer).unwrap();
    assert_eq!(result, Transition::Changed);
    assert_eq!(plan.status, PlanStatus::Complete);
    assert!(plan.updated_at > before);
}

#[test]
fn plan_transition_active_complete_by_reactor() {
    let mut plan = Plan::new("g".to_string());
    let result = plan.transition(PlanStatus::Complete, Role::Reactor).unwrap();
    assert_eq!(result, Transition::Changed);
    assert_eq!(plan.status, PlanStatus::Complete);
}

#[test]
fn plan_transition_active_complete_by_director_rejects() {
    let mut plan = Plan::new("g".to_string());
    let err = plan.transition(PlanStatus::Complete, Role::Director).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::RoleNotAuthorized);
    assert_eq!(plan.status, PlanStatus::Active, "state must not mutate on Err");
}

#[test]
fn plan_transition_no_direct_draft_to_complete() {
    // The FSM has no Draft -> Complete edge (must go through Active first).
    let mut plan = Plan::new("g".to_string());
    plan.status = PlanStatus::Draft;
    let before_updated_at = plan.updated_at;
    let err = plan.transition(PlanStatus::Complete, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
    assert_eq!(plan.status, PlanStatus::Draft);
    assert_eq!(plan.updated_at, before_updated_at);
}

#[test]
fn plan_transition_same_state_is_unchanged() {
    let mut plan = Plan::new("g".to_string());
    let before_updated_at = plan.updated_at;
    let result = plan.transition(PlanStatus::Active, Role::Reactor).unwrap();
    assert_eq!(result, Transition::Unchanged);
    assert_eq!(plan.status, PlanStatus::Active);
    assert_eq!(
        plan.updated_at, before_updated_at,
        "Unchanged must not advance updated_at"
    );
}

// ---------------------------------------------------------------------------
// FSM overrides (Director-only back-edges)
// ---------------------------------------------------------------------------

#[test]
fn plan_override_active_draft_by_director_mutates_state() {
    // Architect Finding #1: the guard in override_status was originally
    // == Transition::Changed, which would never fire for an Override return.
    // This test pins both the returned variant AND the state mutation so a
    // regression to the broken guard fails loudly.
    let mut plan = Plan::new("g".to_string());
    std::thread::sleep(std::time::Duration::from_millis(2));
    let before = plan.updated_at;
    let result = plan.override_status(PlanStatus::Draft, Role::Director).unwrap();
    assert_eq!(result, Transition::Override);
    assert_eq!(plan.status, PlanStatus::Draft, "override must mutate status");
    assert!(plan.updated_at > before, "override must advance updated_at");
}

#[test]
fn plan_override_active_draft_by_reactor_rejects() {
    let mut plan = Plan::new("g".to_string());
    let err = plan.override_status(PlanStatus::Draft, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::RoleNotAuthorized);
    assert_eq!(plan.status, PlanStatus::Active);
}

#[test]
fn plan_override_returns_changed_for_normal_edge() {
    // validate_override falls through to validate_transition first; a valid
    // normal edge (Active -> Complete by Decomposer) should return Changed
    // from the override method, not Override. Confirms the override path is
    // strictly permissive, not exclusive.
    let mut plan = Plan::new("g".to_string());
    let result = plan.override_status(PlanStatus::Complete, Role::Decomposer).unwrap();
    assert_eq!(result, Transition::Changed);
    assert_eq!(plan.status, PlanStatus::Complete);
}

#[test]
fn plan_override_nonexistent_edge_rejects() {
    // Active -> Complete is a normal edge but only for Reactor/Decomposer;
    // there's no override edge for it, so Director (not in the normal 'by' list)
    // should get NoTransition from the override fallthrough path.
    let mut plan = Plan::new("g".to_string());
    let err = plan.override_status(PlanStatus::Complete, Role::Director).unwrap_err();
    // The exact error kind depends on the derive's fallthrough logic; it
    // will surface as either NoTransition or RoleNotAuthorized. Either way
    // the state must not have mutated.
    assert!(err.kind == FsmErrorKind::NoTransition || err.kind == FsmErrorKind::RoleNotAuthorized);
    assert_eq!(plan.status, PlanStatus::Active);
}

// ---------------------------------------------------------------------------
// Terminal state detection
// ---------------------------------------------------------------------------

#[test]
fn plan_status_terminal_states() {
    assert!(PlanStatus::Complete.is_terminal());
    assert!(PlanStatus::Superseded.is_terminal());
    assert!(PlanStatus::Abandoned.is_terminal());
    assert!(!PlanStatus::Draft.is_terminal());
    assert!(!PlanStatus::Pending.is_terminal());
    assert!(!PlanStatus::Active.is_terminal());
    // Stalled is intentionally non-terminal: it has an outgoing
    // `Stalled => Active` override (operator recovery), and the FSM
    // derive forbids terminal states from having outgoing edges.
    assert!(!PlanStatus::Stalled.is_terminal());
}

// ---------------------------------------------------------------------------
// Stalled: retry-budget-exhaustion FSM (Director-only)
// ---------------------------------------------------------------------------

#[test]
fn plan_transition_active_stalled_by_director() {
    let mut plan = Plan::new("g".to_string());
    std::thread::sleep(std::time::Duration::from_millis(2));
    let before = plan.updated_at;
    let result = plan.transition(PlanStatus::Stalled, Role::Director).unwrap();
    assert_eq!(result, Transition::Changed);
    assert_eq!(plan.status, PlanStatus::Stalled);
    assert!(plan.updated_at > before);
}

#[test]
fn plan_transition_active_stalled_by_reactor_succeeds() {
    // The Reactor is authorized for Active -> Stalled (gate-hardening
    // Architect-audit follow-up): the daemon stalls a Plan it cannot
    // decompose so the operator sees a stuck Plan rather than a deceptive
    // Active one with zero Works. Previously Director-only.
    let mut plan = Plan::new("g".to_string());
    let before = plan.updated_at;
    let transition = plan.transition(PlanStatus::Stalled, Role::Reactor).unwrap();
    assert_eq!(transition, Transition::Changed);
    assert_eq!(plan.status, PlanStatus::Stalled);
    assert!(plan.updated_at >= before);
}

#[test]
fn plan_transition_active_stalled_by_decomposer_rejects() {
    let mut plan = Plan::new("g".to_string());
    let err = plan.transition(PlanStatus::Stalled, Role::Decomposer).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::RoleNotAuthorized);
    assert_eq!(plan.status, PlanStatus::Active);
}

#[test]
fn plan_override_stalled_active_by_director() {
    // Operator-recovery path: Stalled -> Active via override only.
    let mut plan = Plan::new("g".to_string());
    plan.transition(PlanStatus::Stalled, Role::Director).unwrap();
    assert_eq!(plan.status, PlanStatus::Stalled);
    std::thread::sleep(std::time::Duration::from_millis(2));
    let before = plan.updated_at;
    let result = plan.override_status(PlanStatus::Active, Role::Director).unwrap();
    assert_eq!(result, Transition::Override);
    assert_eq!(plan.status, PlanStatus::Active);
    assert!(plan.updated_at > before);
}

#[test]
fn plan_transition_stalled_active_rejects() {
    // Stalled -> Active is override-only; the normal transition table
    // does not contain it.
    let mut plan = Plan::new("g".to_string());
    plan.transition(PlanStatus::Stalled, Role::Director).unwrap();
    let err = plan.transition(PlanStatus::Active, Role::Director).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
    assert_eq!(plan.status, PlanStatus::Stalled);
}

#[test]
fn plan_transition_stalled_self_is_unchanged() {
    let mut plan = Plan::new("g".to_string());
    plan.transition(PlanStatus::Stalled, Role::Director).unwrap();
    let before_updated_at = plan.updated_at;
    let result = plan.transition(PlanStatus::Stalled, Role::Director).unwrap();
    assert_eq!(result, Transition::Unchanged);
    assert_eq!(plan.status, PlanStatus::Stalled);
    assert_eq!(plan.updated_at, before_updated_at);
}

#[test]
fn plan_status_stalled_display_lowercase() {
    // Display output (used by Record's index map) must match the serde
    // wire form. Locks the kebab/lowercase contract for the new variant.
    assert_eq!(format!("{}", PlanStatus::Stalled), "stalled");
}
