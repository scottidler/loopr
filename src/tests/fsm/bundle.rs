use super::common::{ALL_ROLES, assert_invalid, assert_valid, new_interp, vt};
use crate::domain::bundle::{Bundle, BundleStatus};
use crate::domain::role::Role;
use crate::domain::transition::Transition;
use crate::fsm::status::FsmStatus;

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

// --- All valid transitions ---

#[test]
fn valid_happy_path() {
    let interp = new_interp();
    let chain = [
        (BundleStatus::Proposed, BundleStatus::Triaged, Role::Coordinator),
        (BundleStatus::Triaged, BundleStatus::Reviewed, Role::Coordinator),
        (BundleStatus::Reviewed, BundleStatus::Accepted, Role::Coordinator),
        (BundleStatus::Accepted, BundleStatus::Integrating, Role::Integrator),
        (BundleStatus::Integrating, BundleStatus::Merged, Role::Integrator),
    ];
    for (from, to, role) in &chain {
        let r = vt(&interp, *from, *to, *role);
        assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
    }
}

#[test]
fn valid_triaged_to_reviewed_by_reviewer() {
    let interp = new_interp();
    let r = vt(&interp, BundleStatus::Triaged, BundleStatus::Reviewed, Role::Reviewer);
    assert_valid("Triaged", "Reviewed (Reviewer)", &r);
}

#[test]
fn valid_integrating_to_rejected() {
    let interp = new_interp();
    let r = vt(
        &interp,
        BundleStatus::Integrating,
        BundleStatus::Rejected,
        Role::Integrator,
    );
    assert_valid("Integrating", "Rejected", &r);
}

#[test]
fn valid_accepted_to_rejected() {
    let interp = new_interp();
    let r = vt(
        &interp,
        BundleStatus::Accepted,
        BundleStatus::Rejected,
        Role::Integrator,
    );
    assert_valid("Accepted", "Rejected (Integrator)", &r);
}

#[test]
fn valid_early_rejection_coordinator() {
    let interp = new_interp();
    for from in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Reviewed] {
        let r = vt(&interp, from, BundleStatus::Rejected, Role::Coordinator);
        assert_valid(format!("{:?}", from), "Rejected (Coordinator)", &r);
    }
}

#[test]
fn valid_early_rejection_reviewer() {
    let interp = new_interp();
    for from in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Reviewed] {
        let r = vt(&interp, from, BundleStatus::Rejected, Role::Reviewer);
        assert_valid(format!("{:?}", from), "Rejected (Reviewer)", &r);
    }
}

#[test]
fn valid_superseded_from_all_non_terminal() {
    let interp = new_interp();
    let non_terminal = [
        BundleStatus::Proposed,
        BundleStatus::Triaged,
        BundleStatus::Reviewed,
        BundleStatus::Accepted,
        BundleStatus::Integrating,
    ];
    for from in &non_terminal {
        let r = vt(&interp, *from, BundleStatus::Superseded, Role::Coordinator);
        assert_valid(format!("{:?}", from), "Superseded", &r);
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

// --- Wrong role tests ---

#[test]
fn wrong_role_proposed_to_triaged() {
    let interp = new_interp();
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = vt(&interp, BundleStatus::Proposed, BundleStatus::Triaged, role);
        assert_invalid("Proposed", format!("Triaged ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_triaged_to_reviewed() {
    let interp = new_interp();
    for role in [Role::Implementer, Role::Researcher, Role::Integrator] {
        let r = vt(&interp, BundleStatus::Triaged, BundleStatus::Reviewed, role);
        assert_invalid("Triaged", format!("Reviewed ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_reviewed_to_accepted() {
    let interp = new_interp();
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = vt(&interp, BundleStatus::Reviewed, BundleStatus::Accepted, role);
        assert_invalid("Reviewed", format!("Accepted ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_accepted_to_integrating() {
    let interp = new_interp();
    for role in [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher] {
        let r = vt(&interp, BundleStatus::Accepted, BundleStatus::Integrating, role);
        assert_invalid("Accepted", format!("Integrating ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_integrating_to_merged() {
    let interp = new_interp();
    for role in [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher] {
        let r = vt(&interp, BundleStatus::Integrating, BundleStatus::Merged, role);
        assert_invalid("Integrating", format!("Merged ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_superseded() {
    let interp = new_interp();
    let non_terminal = [
        BundleStatus::Proposed,
        BundleStatus::Triaged,
        BundleStatus::Reviewed,
        BundleStatus::Accepted,
        BundleStatus::Integrating,
    ];
    for from in &non_terminal {
        for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
            let r = vt(&interp, *from, BundleStatus::Superseded, role);
            assert_invalid(format!("{:?}", from), format!("Superseded ({:?})", role), &r);
        }
    }
}

// --- Skip state tests ---

#[test]
fn skip_states_rejected() {
    let interp = new_interp();
    let skip_pairs = [
        (BundleStatus::Proposed, BundleStatus::Reviewed),
        (BundleStatus::Proposed, BundleStatus::Accepted),
        (BundleStatus::Proposed, BundleStatus::Integrating),
        (BundleStatus::Proposed, BundleStatus::Merged),
        // Triaged->Accepted valid for Coordinator (advisory bypass)
        (BundleStatus::Triaged, BundleStatus::Integrating),
        (BundleStatus::Triaged, BundleStatus::Merged),
        (BundleStatus::Reviewed, BundleStatus::Integrating),
        (BundleStatus::Reviewed, BundleStatus::Merged),
        (BundleStatus::Accepted, BundleStatus::Merged),
    ];
    for (from, to) in &skip_pairs {
        for role in &ALL_ROLES {
            let r = vt(&interp, *from, *to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
    // Advisory bypass: Triaged->Accepted valid for Coordinator only
    assert_valid(
        "Triaged",
        "Accepted (Coordinator)",
        &vt(
            &interp,
            BundleStatus::Triaged,
            BundleStatus::Accepted,
            Role::Coordinator,
        ),
    );
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        assert_invalid(
            "Triaged",
            format!("Accepted ({:?})", role),
            &vt(&interp, BundleStatus::Triaged, BundleStatus::Accepted, role),
        );
    }
}

// --- Reverse direction ---

#[test]
fn reverse_directions_rejected() {
    let interp = new_interp();
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
            let r = vt(&interp, *from, *to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
}

// --- Record serde roundtrip ---

#[test]
#[allow(clippy::unwrap_used)]
fn bundle_serde_all_statuses() {
    for status in &ALL_STATES {
        let mut b = Bundle::new("wi-1".into(), None, "branch".into(), vec!["claims".into()]);
        b.force_status(*status);
        let json = serde_json::to_string(&b).unwrap();
        let restored: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status(), *status);
    }
}

// --- FsmStatus name mapping roundtrip ---

#[test]
fn yaml_name_roundtrip() {
    for status in &ALL_STATES {
        let name = status.to_yaml_name();
        let restored = BundleStatus::from_yaml_name(name).expect("roundtrip failed");
        assert_eq!(*status, restored, "roundtrip failed for {}", name);
    }
}
