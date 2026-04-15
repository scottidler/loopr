use super::common::{ALL_ROLES, assert_invalid, assert_valid, new_interp, vt};
use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::plan::{HierarchyStatus, Plan};
use crate::domain::role::Role;
use crate::domain::transition::Transition;
use crate::fsm::status::FsmStatus;

const ALL_STATES: [HierarchyStatus; 6] = [
    HierarchyStatus::Draft,
    HierarchyStatus::Pending,
    HierarchyStatus::Active,
    HierarchyStatus::Complete,
    HierarchyStatus::Superseded,
    HierarchyStatus::Abandoned,
];

const TERMINAL: [HierarchyStatus; 3] = [
    HierarchyStatus::Complete,
    HierarchyStatus::Superseded,
    HierarchyStatus::Abandoned,
];

// --- Valid transitions: all with Coordinator ---

#[test]
fn valid_draft_to_pending() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Draft,
        HierarchyStatus::Pending,
        Role::Coordinator,
    );
    assert_valid("Draft", "Pending", &r);
}

#[test]
fn valid_draft_to_active() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Draft,
        HierarchyStatus::Active,
        Role::Coordinator,
    );
    assert_valid("Draft", "Active", &r);
}

#[test]
fn valid_pending_to_active() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Pending,
        HierarchyStatus::Active,
        Role::Coordinator,
    );
    assert_valid("Pending", "Active", &r);
}

#[test]
fn valid_pending_to_abandoned() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Pending,
        HierarchyStatus::Abandoned,
        Role::Coordinator,
    );
    assert_valid("Pending", "Abandoned", &r);
}

#[test]
fn valid_active_to_complete() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Active,
        HierarchyStatus::Complete,
        Role::Coordinator,
    );
    assert_valid("Active", "Complete", &r);
}

#[test]
fn valid_draft_to_abandoned() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Draft,
        HierarchyStatus::Abandoned,
        Role::Coordinator,
    );
    assert_valid("Draft", "Abandoned", &r);
}

#[test]
fn valid_active_to_abandoned() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Active,
        HierarchyStatus::Abandoned,
        Role::Coordinator,
    );
    assert_valid("Active", "Abandoned", &r);
}

// --- Wrong role: every valid transition with every wrong role ---

#[test]
fn wrong_role_on_every_valid_transition() {
    let interp = new_interp();
    let valid_pairs = [
        (HierarchyStatus::Draft, HierarchyStatus::Pending),
        (HierarchyStatus::Draft, HierarchyStatus::Active),
        (HierarchyStatus::Pending, HierarchyStatus::Active),
        (HierarchyStatus::Active, HierarchyStatus::Complete),
        (HierarchyStatus::Draft, HierarchyStatus::Superseded),
        (HierarchyStatus::Pending, HierarchyStatus::Superseded),
        (HierarchyStatus::Active, HierarchyStatus::Superseded),
        (HierarchyStatus::Draft, HierarchyStatus::Abandoned),
        (HierarchyStatus::Pending, HierarchyStatus::Abandoned),
        (HierarchyStatus::Active, HierarchyStatus::Abandoned),
    ];
    let wrong_roles = [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator];

    for (from, to) in &valid_pairs {
        for role in &wrong_roles {
            let r = vt(&interp, *from, *to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?}", to), &r);
        }
    }
}

// --- Terminal states: no outbound transitions allowed ---

#[test]
fn terminal_states_reject_all_outbound() {
    let interp = new_interp();
    for terminal in &TERMINAL {
        for target in &ALL_STATES {
            if terminal == target {
                continue;
            }
            for role in &ALL_ROLES {
                let r = vt(&interp, *terminal, *target, *role);
                assert_invalid(format!("{:?}", terminal), format!("{:?}", target), &r);
            }
        }
    }
}

// --- Self-transitions: idempotent ---

#[test]
fn self_transitions_idempotent() {
    let interp = new_interp();
    for state in &ALL_STATES {
        for role in &ALL_ROLES {
            let r = vt(&interp, *state, *state, *role);
            assert_valid(format!("{:?}", state), format!("{:?}", state), &r);
            assert_eq!(r.unwrap(), Transition::Unchanged);
        }
    }
}

// --- Skip states ---

#[test]
fn skip_draft_to_complete_rejected() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Draft,
        HierarchyStatus::Complete,
        Role::Coordinator,
    );
    assert_invalid("Draft", "Complete", &r);
}

#[test]
fn skip_pending_to_complete_rejected() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Pending,
        HierarchyStatus::Complete,
        Role::Coordinator,
    );
    assert_invalid("Pending", "Complete", &r);
}

// --- Reverse direction ---

#[test]
fn reverse_active_to_draft_rejected() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Active,
        HierarchyStatus::Draft,
        Role::Coordinator,
    );
    assert_invalid("Active", "Draft", &r);
}

#[test]
fn reverse_active_to_pending_rejected() {
    let interp = new_interp();
    let r = vt(
        &interp,
        HierarchyStatus::Active,
        HierarchyStatus::Pending,
        Role::Coordinator,
    );
    assert_invalid("Active", "Pending", &r);
}

// --- Record serde roundtrip ---

#[test]
#[allow(clippy::unwrap_used)]
fn plan_serde_all_statuses() {
    for status in &ALL_STATES {
        let mut plan = Plan::new("T".into(), AcceptanceCriteria::default());
        plan.force_status(*status);
        let json = serde_json::to_string(&plan).unwrap();
        let restored: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status(), *status);
        assert_eq!(restored.id, plan.id);
    }
}

// --- FsmStatus name mapping roundtrip ---

#[test]
fn yaml_name_roundtrip() {
    for status in &ALL_STATES {
        let name = status.to_yaml_name();
        let restored = HierarchyStatus::from_yaml_name(name).expect("roundtrip failed");
        assert_eq!(*status, restored, "roundtrip failed for {}", name);
    }
}

// --- Superseded transition tests ---

#[test]
fn valid_superseded_from_all_pre_terminal() {
    let interp = new_interp();
    let pre_terminal = [
        HierarchyStatus::Draft,
        HierarchyStatus::Pending,
        HierarchyStatus::Active,
    ];
    for from in &pre_terminal {
        let r = vt(&interp, *from, HierarchyStatus::Superseded, Role::Coordinator);
        assert_valid(format!("{:?}", from), "Superseded", &r);
    }
}

#[test]
fn wrong_role_superseded() {
    let interp = new_interp();
    let pre_terminal = [
        HierarchyStatus::Draft,
        HierarchyStatus::Pending,
        HierarchyStatus::Active,
    ];
    let wrong_roles = [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator];
    for from in &pre_terminal {
        for role in &wrong_roles {
            let r = vt(&interp, *from, HierarchyStatus::Superseded, *role);
            assert_invalid(format!("{:?}", from), format!("Superseded ({:?})", role), &r);
        }
    }
}

#[test]
fn superseded_is_terminal() {
    let interp = new_interp();
    assert!(HierarchyStatus::Superseded.is_terminal(&interp));
}

#[test]
fn no_transitions_from_superseded() {
    let interp = new_interp();
    for target in &ALL_STATES {
        if *target == HierarchyStatus::Superseded {
            continue;
        }
        for role in &ALL_ROLES {
            let r = vt(&interp, HierarchyStatus::Superseded, *target, *role);
            assert_invalid("Superseded", format!("{:?}", target), &r);
        }
    }
}
