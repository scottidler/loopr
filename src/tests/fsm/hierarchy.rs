use super::common::{ALL_ROLES, assert_invalid, assert_valid};
use crate::domain::plan::{HierarchyStatus, Plan};
use crate::domain::role::Role;
use crate::domain::transition::Transition;

const ALL_STATES: [HierarchyStatus; 4] = [
    HierarchyStatus::Draft,
    HierarchyStatus::Active,
    HierarchyStatus::Complete,
    HierarchyStatus::Abandoned,
];

const TERMINAL: [HierarchyStatus; 2] = [HierarchyStatus::Complete, HierarchyStatus::Abandoned];

// --- Valid transitions: all 4 with Coordinator ---

#[test]
fn valid_draft_to_active() {
    let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Active, Role::Coordinator);
    assert_valid("Draft", "Active", &r);
}

#[test]
fn valid_active_to_complete() {
    let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Complete, Role::Coordinator);
    assert_valid("Active", "Complete", &r);
}

#[test]
fn valid_draft_to_abandoned() {
    let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Abandoned, Role::Coordinator);
    assert_valid("Draft", "Abandoned", &r);
}

#[test]
fn valid_active_to_abandoned() {
    let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Abandoned, Role::Coordinator);
    assert_valid("Active", "Abandoned", &r);
}

// --- Wrong role: every valid transition with every wrong role ---

#[test]
fn wrong_role_on_every_valid_transition() {
    let valid_pairs = [
        (HierarchyStatus::Draft, HierarchyStatus::Active),
        (HierarchyStatus::Active, HierarchyStatus::Complete),
        (HierarchyStatus::Draft, HierarchyStatus::Abandoned),
        (HierarchyStatus::Active, HierarchyStatus::Abandoned),
    ];
    let wrong_roles = [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator];

    for (from, to) in &valid_pairs {
        for role in &wrong_roles {
            let r = from.validate_transition(*to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?}", to), &r);
        }
    }
}

// --- Terminal states: no outbound transitions allowed ---

#[test]
fn terminal_states_reject_all_outbound() {
    for terminal in &TERMINAL {
        for target in &ALL_STATES {
            if terminal == target {
                continue;
            }
            for role in &ALL_ROLES {
                let r = terminal.validate_transition(*target, *role);
                assert_invalid(format!("{:?}", terminal), format!("{:?}", target), &r);
            }
        }
    }
}

// --- Self-transitions: idempotent ---

#[test]
fn self_transitions_idempotent() {
    for state in &ALL_STATES {
        for role in &ALL_ROLES {
            let r = state.validate_transition(*state, *role);
            assert_valid(format!("{:?}", state), format!("{:?}", state), &r);
            assert_eq!(r.unwrap(), Transition::Unchanged);
        }
    }
}

// --- Skip states ---

#[test]
fn skip_draft_to_complete_rejected() {
    let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Complete, Role::Coordinator);
    assert_invalid("Draft", "Complete", &r);
}

// --- Reverse direction ---

#[test]
fn reverse_active_to_draft_rejected() {
    let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Draft, Role::Coordinator);
    assert_invalid("Active", "Draft", &r);
}

// --- Record serde roundtrip ---

#[test]
fn plan_serde_all_statuses() {
    for status in &ALL_STATES {
        let mut plan = Plan::new("T".into(), "C".into());
        plan.force_status(*status);
        let json = serde_json::to_string(&plan).unwrap();
        let restored: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status(), *status);
        assert_eq!(restored.id, plan.id);
    }
}
