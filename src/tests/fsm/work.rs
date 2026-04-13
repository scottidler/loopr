use super::common::{ALL_ROLES, assert_invalid, assert_valid, new_interp, vo, vt};
use crate::domain::role::Role;
use crate::domain::transition::Transition;
use crate::domain::work::{Work, WorkStatus};
use crate::fsm::status::FsmStatus;

const ALL_STATES: [WorkStatus; 9] = [
    WorkStatus::Draft,
    WorkStatus::Pending,
    WorkStatus::Ready,
    WorkStatus::InProgress,
    WorkStatus::Blocked,
    WorkStatus::InReview,
    WorkStatus::Integrated,
    WorkStatus::Done,
    WorkStatus::Abandoned,
];

const TERMINAL: [WorkStatus; 2] = [WorkStatus::Done, WorkStatus::Abandoned];

// --- Valid transitions ---

#[test]
fn valid_draft_to_pending() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::Draft, WorkStatus::Pending, Role::Coordinator);
    assert_valid("Draft", "Pending", &r);
}

#[test]
fn valid_draft_to_ready() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::Draft, WorkStatus::Ready, Role::Coordinator);
    assert_valid("Draft", "Ready", &r);
}

#[test]
fn valid_pending_to_ready() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::Pending, WorkStatus::Ready, Role::Coordinator);
    assert_valid("Pending", "Ready", &r);
}

#[test]
fn valid_pending_to_abandoned() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::Pending, WorkStatus::Abandoned, Role::Coordinator);
    assert_valid("Pending", "Abandoned", &r);
}

#[test]
fn valid_ready_to_in_progress() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::Ready, WorkStatus::InProgress, Role::Coordinator);
    assert_valid("Ready", "InProgress", &r);
}

#[test]
fn valid_in_progress_to_blocked_any_role() {
    let interp = new_interp();
    for role in &ALL_ROLES {
        let r = vt(&interp, WorkStatus::InProgress, WorkStatus::Blocked, *role);
        assert_valid("InProgress", format!("Blocked ({:?})", role), &r);
    }
}

#[test]
fn valid_blocked_to_ready() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::Blocked, WorkStatus::Ready, Role::Coordinator);
    assert_valid("Blocked", "Ready", &r);
}

#[test]
fn valid_in_progress_to_in_review() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::InProgress, WorkStatus::InReview, Role::Implementer);
    assert_valid("InProgress", "InReview", &r);
}

#[test]
fn valid_in_review_to_in_progress() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::InReview, WorkStatus::InProgress, Role::Coordinator);
    assert_valid("InReview", "InProgress", &r);
}

#[test]
fn valid_in_review_to_integrated() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::InReview, WorkStatus::Integrated, Role::Integrator);
    assert_valid("InReview", "Integrated", &r);
}

#[test]
fn valid_integrated_to_done_coordinator() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::Integrated, WorkStatus::Done, Role::Coordinator);
    assert_valid("Integrated", "Done (Coordinator)", &r);
}

#[test]
fn valid_integrated_to_done_integrator() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::Integrated, WorkStatus::Done, Role::Integrator);
    assert_valid("Integrated", "Done (Integrator)", &r);
}

#[test]
fn valid_abandoned_from_all_non_terminal() {
    let interp = new_interp();
    let non_terminal = [
        WorkStatus::Draft,
        WorkStatus::Pending,
        WorkStatus::Ready,
        WorkStatus::InProgress,
        WorkStatus::Blocked,
        WorkStatus::InReview,
        WorkStatus::Integrated,
    ];
    for from in &non_terminal {
        let r = vt(&interp, *from, WorkStatus::Abandoned, Role::Coordinator);
        assert_valid(format!("{:?}", from), "Abandoned", &r);
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
fn wrong_role_draft_to_ready() {
    let interp = new_interp();
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = vt(&interp, WorkStatus::Draft, WorkStatus::Ready, role);
        assert_invalid("Draft", format!("Ready ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_ready_to_in_progress() {
    let interp = new_interp();
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = vt(&interp, WorkStatus::Ready, WorkStatus::InProgress, role);
        assert_invalid("Ready", format!("InProgress ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_in_progress_to_in_review() {
    let interp = new_interp();
    for role in [Role::Coordinator, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = vt(&interp, WorkStatus::InProgress, WorkStatus::InReview, role);
        assert_invalid("InProgress", format!("InReview ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_in_review_to_in_progress() {
    let interp = new_interp();
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = vt(&interp, WorkStatus::InReview, WorkStatus::InProgress, role);
        assert_invalid("InReview", format!("InProgress ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_in_review_to_integrated() {
    let interp = new_interp();
    for role in [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher] {
        let r = vt(&interp, WorkStatus::InReview, WorkStatus::Integrated, role);
        assert_invalid("InReview", format!("Integrated ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_integrated_to_done() {
    let interp = new_interp();
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher] {
        let r = vt(&interp, WorkStatus::Integrated, WorkStatus::Done, role);
        assert_invalid("Integrated", format!("Done ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_abandoned() {
    let interp = new_interp();
    let non_terminal = [
        WorkStatus::Draft,
        WorkStatus::Pending,
        WorkStatus::Ready,
        WorkStatus::InProgress,
        WorkStatus::Blocked,
        WorkStatus::InReview,
        WorkStatus::Integrated,
    ];
    for from in &non_terminal {
        for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
            let r = vt(&interp, *from, WorkStatus::Abandoned, role);
            assert_invalid(format!("{:?}", from), format!("Abandoned ({:?})", role), &r);
        }
    }
}

// --- Skip state tests ---

#[test]
fn skip_states_rejected() {
    let interp = new_interp();
    // Ready->Done(Coordinator) is valid as the pre-flight AC short-circuit.
    let skip_pairs = [
        (WorkStatus::Draft, WorkStatus::InProgress),
        (WorkStatus::Draft, WorkStatus::InReview),
        (WorkStatus::Draft, WorkStatus::Integrated),
        (WorkStatus::Draft, WorkStatus::Done),
        (WorkStatus::Pending, WorkStatus::InProgress),
        (WorkStatus::Pending, WorkStatus::InReview),
        (WorkStatus::Pending, WorkStatus::Integrated),
        (WorkStatus::Pending, WorkStatus::Done),
        (WorkStatus::Ready, WorkStatus::InReview),
        (WorkStatus::Ready, WorkStatus::Integrated),
        (WorkStatus::Blocked, WorkStatus::InProgress),
        (WorkStatus::Blocked, WorkStatus::InReview),
        (WorkStatus::Blocked, WorkStatus::Integrated),
        (WorkStatus::Blocked, WorkStatus::Done),
        (WorkStatus::InProgress, WorkStatus::Integrated),
        (WorkStatus::InProgress, WorkStatus::Done),
        (WorkStatus::InReview, WorkStatus::Done),
    ];
    for (from, to) in &skip_pairs {
        for role in &ALL_ROLES {
            let r = vt(&interp, *from, *to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
}

#[test]
fn valid_ready_to_done_coordinator() {
    let interp = new_interp();
    let r = vt(&interp, WorkStatus::Ready, WorkStatus::Done, Role::Coordinator);
    assert_valid("Ready", "Done (Coordinator)", &r);
}

#[test]
fn wrong_role_ready_to_done() {
    let interp = new_interp();
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = vt(&interp, WorkStatus::Ready, WorkStatus::Done, role);
        assert_invalid("Ready", format!("Done ({:?})", role), &r);
    }
}

// --- Reverse direction ---

#[test]
fn reverse_directions_rejected() {
    let interp = new_interp();
    let reverse_pairs = [
        (WorkStatus::Pending, WorkStatus::Draft),
        (WorkStatus::Ready, WorkStatus::Draft),
        (WorkStatus::Ready, WorkStatus::Pending),
        (WorkStatus::InProgress, WorkStatus::Draft),
        (WorkStatus::InProgress, WorkStatus::Pending),
        (WorkStatus::InProgress, WorkStatus::Ready),
        (WorkStatus::Integrated, WorkStatus::InReview),
        (WorkStatus::Integrated, WorkStatus::InProgress),
        (WorkStatus::Integrated, WorkStatus::Ready),
        (WorkStatus::Integrated, WorkStatus::Draft),
        (WorkStatus::Done, WorkStatus::Integrated),
    ];
    for (from, to) in &reverse_pairs {
        for role in &ALL_ROLES {
            let r = vt(&interp, *from, *to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
}

// --- Full happy-path lifecycle ---

#[test]
fn full_lifecycle_happy_path() {
    let interp = new_interp();
    let chain: Vec<(WorkStatus, WorkStatus, Role)> = vec![
        (WorkStatus::Draft, WorkStatus::Pending, Role::Coordinator),
        (WorkStatus::Pending, WorkStatus::Ready, Role::Coordinator),
        (WorkStatus::Ready, WorkStatus::InProgress, Role::Coordinator),
        (WorkStatus::InProgress, WorkStatus::InReview, Role::Implementer),
        (WorkStatus::InReview, WorkStatus::Integrated, Role::Integrator),
        (WorkStatus::Integrated, WorkStatus::Done, Role::Coordinator),
    ];
    for (from, to, role) in &chain {
        let r = vt(&interp, *from, *to, *role);
        assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
    }
}

// --- Override transitions ---

#[test]
fn override_in_progress_to_ready_coordinator() {
    let interp = new_interp();
    let r = vo(&interp, WorkStatus::InProgress, WorkStatus::Ready, Role::Coordinator);
    assert_valid("InProgress", "Ready (override)", &r);
}

#[test]
fn override_in_progress_to_in_review_coordinator() {
    let interp = new_interp();
    let r = vo(&interp, WorkStatus::InProgress, WorkStatus::InReview, Role::Coordinator);
    assert_valid("InProgress", "InReview (override)", &r);
}

#[test]
fn override_in_review_to_ready_coordinator() {
    let interp = new_interp();
    let r = vo(&interp, WorkStatus::InReview, WorkStatus::Ready, Role::Coordinator);
    assert_valid("InReview", "Ready (override)", &r);
}

#[test]
fn override_rejected_for_wrong_roles() {
    let interp = new_interp();
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = vo(&interp, WorkStatus::InProgress, WorkStatus::Ready, role);
        assert_invalid("InProgress", format!("Ready override ({:?})", role), &r);
    }
}

#[test]
fn override_in_progress_to_ready_not_normal_transition() {
    let interp = new_interp();
    // Normal transition should fail; override should succeed
    assert_invalid(
        "InProgress",
        "Ready (normal)",
        &vt(&interp, WorkStatus::InProgress, WorkStatus::Ready, Role::Coordinator),
    );
    assert_valid(
        "InProgress",
        "Ready (override)",
        &vo(&interp, WorkStatus::InProgress, WorkStatus::Ready, Role::Coordinator),
    );
}

// --- Record serde roundtrip ---

#[test]
#[allow(clippy::unwrap_used)]
fn work_serde_all_statuses() {
    for status in &ALL_STATES {
        let mut wi = Work::new("ph-1".into(), "T".into());
        wi.force_status(*status);
        let json = serde_json::to_string(&wi).unwrap();
        let restored: Work = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status(), *status);
    }
}

// --- FsmStatus name mapping roundtrip ---

#[test]
fn yaml_name_roundtrip() {
    for status in &ALL_STATES {
        let name = status.to_yaml_name();
        let restored = WorkStatus::from_yaml_name(name).expect("roundtrip failed");
        assert_eq!(*status, restored, "roundtrip failed for {}", name);
    }
}
