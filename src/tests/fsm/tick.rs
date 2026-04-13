use super::common::{ALL_ROLES, assert_invalid, assert_valid, new_interp, vt};
use crate::domain::role::Role;
use crate::domain::tick::{Tick, TickStatus};
use crate::domain::transition::Transition;
use crate::fsm::status::FsmStatus;

const ALL_STATES: [TickStatus; 5] = [
    TickStatus::Open,
    TickStatus::Sealing,
    TickStatus::Validating,
    TickStatus::Published,
    TickStatus::Failed,
];

const TERMINAL: [TickStatus; 2] = [TickStatus::Published, TickStatus::Failed];

// --- All valid transitions ---

#[test]
fn valid_open_to_sealing() {
    let interp = new_interp();
    let r = vt(&interp, TickStatus::Open, TickStatus::Sealing, Role::Integrator);
    assert_valid("Open", "Sealing", &r);
}

#[test]
fn valid_sealing_to_validating() {
    let interp = new_interp();
    let r = vt(&interp, TickStatus::Sealing, TickStatus::Validating, Role::Integrator);
    assert_valid("Sealing", "Validating", &r);
}

#[test]
fn valid_validating_to_published() {
    let interp = new_interp();
    let r = vt(&interp, TickStatus::Validating, TickStatus::Published, Role::Integrator);
    assert_valid("Validating", "Published", &r);
}

#[test]
fn valid_open_to_failed() {
    let interp = new_interp();
    let r = vt(&interp, TickStatus::Open, TickStatus::Failed, Role::Integrator);
    assert_valid("Open", "Failed", &r);
}

#[test]
fn valid_sealing_to_failed() {
    let interp = new_interp();
    let r = vt(&interp, TickStatus::Sealing, TickStatus::Failed, Role::Integrator);
    assert_valid("Sealing", "Failed", &r);
}

#[test]
fn valid_validating_to_failed() {
    let interp = new_interp();
    let r = vt(&interp, TickStatus::Validating, TickStatus::Failed, Role::Integrator);
    assert_valid("Validating", "Failed", &r);
}

// --- Wrong role on every valid transition ---

#[test]
fn wrong_role_on_every_valid_transition() {
    let interp = new_interp();
    let valid_pairs = [
        (TickStatus::Open, TickStatus::Sealing),
        (TickStatus::Open, TickStatus::Failed),
        (TickStatus::Sealing, TickStatus::Validating),
        (TickStatus::Sealing, TickStatus::Failed),
        (TickStatus::Validating, TickStatus::Published),
        (TickStatus::Validating, TickStatus::Failed),
    ];
    let wrong_roles = [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher];

    for (from, to) in &valid_pairs {
        for role in &wrong_roles {
            let r = vt(&interp, *from, *to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
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
fn skip_states_rejected() {
    let interp = new_interp();
    let skip_pairs = [
        (TickStatus::Open, TickStatus::Validating),
        (TickStatus::Open, TickStatus::Published),
        // Open->Failed is valid (crash recovery)
        (TickStatus::Sealing, TickStatus::Published),
        // Sealing->Failed is valid (B3: merge failure)
    ];
    for (from, to) in &skip_pairs {
        for role in &ALL_ROLES {
            let r = vt(&interp, *from, *to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
}

// --- Reverse direction ---

#[test]
fn reverse_directions_rejected() {
    let interp = new_interp();
    let reverse_pairs = [
        (TickStatus::Sealing, TickStatus::Open),
        (TickStatus::Validating, TickStatus::Sealing),
        (TickStatus::Validating, TickStatus::Open),
    ];
    for (from, to) in &reverse_pairs {
        for role in &ALL_ROLES {
            let r = vt(&interp, *from, *to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
}

// --- is_terminal() via interpreter ---

#[test]
fn is_terminal_correct() {
    let interp = new_interp();
    use crate::fsm::status::FsmStatus;
    assert!(!TickStatus::Open.is_terminal(&interp));
    assert!(!TickStatus::Sealing.is_terminal(&interp));
    assert!(!TickStatus::Validating.is_terminal(&interp));
    assert!(TickStatus::Published.is_terminal(&interp));
    assert!(TickStatus::Failed.is_terminal(&interp));
}

// --- Record serde roundtrip ---

#[test]
#[allow(clippy::unwrap_used)]
fn tick_serde_all_statuses() {
    for status in &ALL_STATES {
        let mut t = Tick::new(1);
        t.force_status(*status);
        let json = serde_json::to_string(&t).unwrap();
        let restored: Tick = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status(), *status);
    }
}

// --- Full lifecycle ---

#[test]
fn full_lifecycle_happy_path() {
    let interp = new_interp();
    let chain = [
        (TickStatus::Open, TickStatus::Sealing),
        (TickStatus::Sealing, TickStatus::Validating),
        (TickStatus::Validating, TickStatus::Published),
    ];
    for (from, to) in &chain {
        let r = vt(&interp, *from, *to, Role::Integrator);
        assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
    }
}

#[test]
fn full_lifecycle_failure_path() {
    let interp = new_interp();
    let chain = [
        (TickStatus::Open, TickStatus::Sealing),
        (TickStatus::Sealing, TickStatus::Validating),
        (TickStatus::Validating, TickStatus::Failed),
    ];
    for (from, to) in &chain {
        let r = vt(&interp, *from, *to, Role::Integrator);
        assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
    }
}

// --- FsmStatus name mapping roundtrip ---

#[test]
fn yaml_name_roundtrip() {
    for status in &ALL_STATES {
        let name = status.to_yaml_name();
        let restored = TickStatus::from_yaml_name(name).expect("roundtrip failed");
        assert_eq!(*status, restored, "roundtrip failed for {}", name);
    }
}
