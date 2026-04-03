use super::common::{ALL_ROLES, assert_invalid, assert_valid};
use crate::domain::bundle::{Bundle, BundleStatus};
use crate::domain::role::Role;
use crate::domain::transition::Transition;

const ALL_STATES: [BundleStatus; 8] = [
    BundleStatus::Proposed,
    BundleStatus::Triaged,
    BundleStatus::Reviewed,
    BundleStatus::Accepted,
    BundleStatus::Integrating,
    BundleStatus::Merged,
    BundleStatus::Rejected,
    BundleStatus::Superseded,
];

const TERMINAL: [BundleStatus; 3] = [BundleStatus::Merged, BundleStatus::Rejected, BundleStatus::Superseded];

// --- All 19 valid transitions ---

#[test]
fn valid_happy_path() {
    let chain = [
        (BundleStatus::Proposed, BundleStatus::Triaged, Role::Coordinator),
        (BundleStatus::Triaged, BundleStatus::Reviewed, Role::Coordinator),
        (BundleStatus::Reviewed, BundleStatus::Accepted, Role::Coordinator),
        (BundleStatus::Accepted, BundleStatus::Integrating, Role::Integrator),
        (BundleStatus::Integrating, BundleStatus::Merged, Role::Integrator),
    ];
    for (from, to, role) in &chain {
        let r = from.validate_transition(*to, *role);
        assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
    }
}

#[test]
fn valid_triaged_to_reviewed_by_reviewer() {
    let r = BundleStatus::Triaged.validate_transition(BundleStatus::Reviewed, Role::Reviewer);
    assert_valid("Triaged", "Reviewed (Reviewer)", &r);
}

#[test]
fn valid_integrating_to_rejected() {
    let r = BundleStatus::Integrating.validate_transition(BundleStatus::Rejected, Role::Integrator);
    assert_valid("Integrating", "Rejected", &r);
}

#[test]
fn valid_accepted_to_rejected() {
    let r = BundleStatus::Accepted.validate_transition(BundleStatus::Rejected, Role::Integrator);
    assert_valid("Accepted", "Rejected (Integrator)", &r);
}

#[test]
fn valid_early_rejection_coordinator() {
    for from in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Reviewed] {
        let r = from.validate_transition(BundleStatus::Rejected, Role::Coordinator);
        assert_valid(format!("{:?}", from), "Rejected (Coordinator)", &r);
    }
}

#[test]
fn valid_early_rejection_reviewer() {
    for from in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Reviewed] {
        let r = from.validate_transition(BundleStatus::Rejected, Role::Reviewer);
        assert_valid(format!("{:?}", from), "Rejected (Reviewer)", &r);
    }
}

#[test]
fn valid_superseded_from_all_non_terminal() {
    let non_terminal = [
        BundleStatus::Proposed,
        BundleStatus::Triaged,
        BundleStatus::Reviewed,
        BundleStatus::Accepted,
        BundleStatus::Integrating,
    ];
    for from in &non_terminal {
        let r = from.validate_transition(BundleStatus::Superseded, Role::Coordinator);
        assert_valid(format!("{:?}", from), "Superseded", &r);
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

// --- Wrong role tests ---

#[test]
fn wrong_role_proposed_to_triaged() {
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = BundleStatus::Proposed.validate_transition(BundleStatus::Triaged, role);
        assert_invalid("Proposed", format!("Triaged ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_triaged_to_reviewed() {
    // Only Coordinator and Reviewer are valid
    for role in [Role::Implementer, Role::Researcher, Role::Integrator] {
        let r = BundleStatus::Triaged.validate_transition(BundleStatus::Reviewed, role);
        assert_invalid("Triaged", format!("Reviewed ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_reviewed_to_accepted() {
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = BundleStatus::Reviewed.validate_transition(BundleStatus::Accepted, role);
        assert_invalid("Reviewed", format!("Accepted ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_accepted_to_integrating() {
    for role in [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher] {
        let r = BundleStatus::Accepted.validate_transition(BundleStatus::Integrating, role);
        assert_invalid("Accepted", format!("Integrating ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_integrating_to_merged() {
    for role in [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher] {
        let r = BundleStatus::Integrating.validate_transition(BundleStatus::Merged, role);
        assert_invalid("Integrating", format!("Merged ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_superseded() {
    let non_terminal = [
        BundleStatus::Proposed,
        BundleStatus::Triaged,
        BundleStatus::Reviewed,
        BundleStatus::Accepted,
        BundleStatus::Integrating,
    ];
    for from in &non_terminal {
        for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
            let r = from.validate_transition(BundleStatus::Superseded, role);
            assert_invalid(format!("{:?}", from), format!("Superseded ({:?})", role), &r);
        }
    }
}

// --- Skip state tests ---

#[test]
fn skip_states_rejected() {
    let skip_pairs = [
        (BundleStatus::Proposed, BundleStatus::Reviewed),
        (BundleStatus::Proposed, BundleStatus::Accepted),
        (BundleStatus::Proposed, BundleStatus::Integrating),
        (BundleStatus::Proposed, BundleStatus::Merged),
        // Triaged->Accepted is now valid for Coordinator (advisory review bypass)
        (BundleStatus::Triaged, BundleStatus::Integrating),
        (BundleStatus::Triaged, BundleStatus::Merged),
        (BundleStatus::Reviewed, BundleStatus::Integrating),
        (BundleStatus::Reviewed, BundleStatus::Merged),
        (BundleStatus::Accepted, BundleStatus::Merged),
    ];
    for (from, to) in &skip_pairs {
        for role in &ALL_ROLES {
            let r = from.validate_transition(*to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
    // Triaged->Accepted: valid for Coordinator only
    let r = BundleStatus::Triaged.validate_transition(BundleStatus::Accepted, Role::Coordinator);
    assert_valid("Triaged", "Accepted (Coordinator)", &r);
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = BundleStatus::Triaged.validate_transition(BundleStatus::Accepted, role);
        assert_invalid("Triaged", format!("Accepted ({:?})", role), &r);
    }
}

// --- Reverse direction ---

#[test]
fn reverse_directions_rejected() {
    let reverse_pairs = [
        (BundleStatus::Triaged, BundleStatus::Proposed),
        (BundleStatus::Reviewed, BundleStatus::Triaged),
        (BundleStatus::Reviewed, BundleStatus::Proposed),
        (BundleStatus::Accepted, BundleStatus::Reviewed),
        (BundleStatus::Accepted, BundleStatus::Triaged),
        (BundleStatus::Accepted, BundleStatus::Proposed),
        (BundleStatus::Integrating, BundleStatus::Accepted),
        (BundleStatus::Integrating, BundleStatus::Reviewed),
        (BundleStatus::Integrating, BundleStatus::Triaged),
        (BundleStatus::Integrating, BundleStatus::Proposed),
    ];
    for (from, to) in &reverse_pairs {
        for role in &ALL_ROLES {
            let r = from.validate_transition(*to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
}

// --- Record serde roundtrip ---

#[test]
fn bundle_serde_all_statuses() {
    for status in &ALL_STATES {
        let mut b = Bundle::new("wi-1".into(), None, "branch".into(), vec!["claims".into()]);
        b.force_status(*status);
        let json = serde_json::to_string(&b).unwrap();
        let restored: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status(), *status);
    }
}
