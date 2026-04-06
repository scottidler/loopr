use super::common::{ALL_ROLES, assert_invalid, assert_valid};
use crate::domain::role::Role;
use crate::domain::transition::Transition;
use crate::domain::work::{Work, WorkStatus};

const ALL_STATES: [WorkStatus; 8] = [
    WorkStatus::Draft,
    WorkStatus::Ready,
    WorkStatus::InProgress,
    WorkStatus::Blocked,
    WorkStatus::InReview,
    WorkStatus::Integrated,
    WorkStatus::Done,
    WorkStatus::Abandoned,
];

const TERMINAL: [WorkStatus; 2] = [WorkStatus::Done, WorkStatus::Abandoned];

// --- All 15 valid transitions ---

#[test]
fn valid_draft_to_ready() {
    let r = WorkStatus::Draft.validate_transition(WorkStatus::Ready, Role::Coordinator);
    assert_valid("Draft", "Ready", &r);
}

#[test]
fn valid_ready_to_in_progress() {
    let r = WorkStatus::Ready.validate_transition(WorkStatus::InProgress, Role::Coordinator);
    assert_valid("Ready", "InProgress", &r);
}

#[test]
fn valid_in_progress_to_blocked_any_role() {
    for role in &ALL_ROLES {
        let r = WorkStatus::InProgress.validate_transition(WorkStatus::Blocked, *role);
        assert_valid("InProgress", format!("Blocked ({:?})", role), &r);
    }
}

#[test]
fn valid_blocked_to_ready() {
    let r = WorkStatus::Blocked.validate_transition(WorkStatus::Ready, Role::Coordinator);
    assert_valid("Blocked", "Ready", &r);
}

#[test]
fn valid_in_progress_to_in_review() {
    let r = WorkStatus::InProgress.validate_transition(WorkStatus::InReview, Role::Implementer);
    assert_valid("InProgress", "InReview", &r);
}

#[test]
fn valid_in_review_to_in_progress() {
    let r = WorkStatus::InReview.validate_transition(WorkStatus::InProgress, Role::Coordinator);
    assert_valid("InReview", "InProgress", &r);
}

#[test]
fn valid_in_review_to_integrated() {
    let r = WorkStatus::InReview.validate_transition(WorkStatus::Integrated, Role::Integrator);
    assert_valid("InReview", "Integrated", &r);
}

#[test]
fn valid_integrated_to_done_coordinator() {
    let r = WorkStatus::Integrated.validate_transition(WorkStatus::Done, Role::Coordinator);
    assert_valid("Integrated", "Done (Coordinator)", &r);
}

#[test]
fn valid_integrated_to_done_integrator() {
    let r = WorkStatus::Integrated.validate_transition(WorkStatus::Done, Role::Integrator);
    assert_valid("Integrated", "Done (Integrator)", &r);
}

#[test]
fn valid_abandoned_from_all_non_terminal() {
    let non_terminal = [
        WorkStatus::Draft,
        WorkStatus::Ready,
        WorkStatus::InProgress,
        WorkStatus::Blocked,
        WorkStatus::InReview,
        WorkStatus::Integrated,
    ];
    for from in &non_terminal {
        let r = from.validate_transition(WorkStatus::Abandoned, Role::Coordinator);
        assert_valid(format!("{:?}", from), "Abandoned", &r);
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
fn wrong_role_draft_to_ready() {
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = WorkStatus::Draft.validate_transition(WorkStatus::Ready, role);
        assert_invalid("Draft", format!("Ready ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_ready_to_in_progress() {
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = WorkStatus::Ready.validate_transition(WorkStatus::InProgress, role);
        assert_invalid("Ready", format!("InProgress ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_in_progress_to_in_review() {
    for role in [Role::Coordinator, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = WorkStatus::InProgress.validate_transition(WorkStatus::InReview, role);
        assert_invalid("InProgress", format!("InReview ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_in_review_to_in_progress() {
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = WorkStatus::InReview.validate_transition(WorkStatus::InProgress, role);
        assert_invalid("InReview", format!("InProgress ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_in_review_to_integrated() {
    for role in [Role::Coordinator, Role::Implementer, Role::Reviewer, Role::Researcher] {
        let r = WorkStatus::InReview.validate_transition(WorkStatus::Integrated, role);
        assert_invalid("InReview", format!("Integrated ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_integrated_to_done() {
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher] {
        let r = WorkStatus::Integrated.validate_transition(WorkStatus::Done, role);
        assert_invalid("Integrated", format!("Done ({:?})", role), &r);
    }
}

#[test]
fn wrong_role_abandoned() {
    let non_terminal = [
        WorkStatus::Draft,
        WorkStatus::Ready,
        WorkStatus::InProgress,
        WorkStatus::Blocked,
        WorkStatus::InReview,
        WorkStatus::Integrated,
    ];
    for from in &non_terminal {
        for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
            let r = from.validate_transition(WorkStatus::Abandoned, role);
            assert_invalid(format!("{:?}", from), format!("Abandoned ({:?})", role), &r);
        }
    }
}

// --- Skip state tests ---

#[test]
fn skip_states_rejected() {
    // (WorkStatus::Ready, WorkStatus::Done) is intentionally absent: Ready->Done(Coordinator)
    // is now valid as the pre-flight AC short-circuit path.
    let skip_pairs = [
        (WorkStatus::Draft, WorkStatus::InProgress),
        (WorkStatus::Draft, WorkStatus::InReview),
        (WorkStatus::Draft, WorkStatus::Integrated),
        (WorkStatus::Draft, WorkStatus::Done),
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
            let r = from.validate_transition(*to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
}

#[test]
fn valid_ready_to_done_coordinator() {
    // Pre-flight AC short-circuit: Coordinator can mark work Done without implementing
    let r = WorkStatus::Ready.validate_transition(WorkStatus::Done, Role::Coordinator);
    assert_valid("Ready", "Done (Coordinator)", &r);
}

#[test]
fn wrong_role_ready_to_done() {
    // Only Coordinator can use the pre-flight short-circuit
    for role in [Role::Implementer, Role::Reviewer, Role::Researcher, Role::Integrator] {
        let r = WorkStatus::Ready.validate_transition(WorkStatus::Done, role);
        assert_invalid("Ready", format!("Done ({:?})", role), &r);
    }
}

// --- Reverse direction ---

#[test]
fn reverse_directions_rejected() {
    let reverse_pairs = [
        (WorkStatus::Ready, WorkStatus::Draft),
        (WorkStatus::InProgress, WorkStatus::Draft),
        (WorkStatus::InProgress, WorkStatus::Ready),
        (WorkStatus::Integrated, WorkStatus::InReview),
        (WorkStatus::Integrated, WorkStatus::InProgress),
        (WorkStatus::Integrated, WorkStatus::Ready),
        (WorkStatus::Integrated, WorkStatus::Draft),
        (WorkStatus::Done, WorkStatus::Integrated),
    ];
    for (from, to) in &reverse_pairs {
        for role in &ALL_ROLES {
            let r = from.validate_transition(*to, *role);
            assert_invalid(format!("{:?}", from), format!("{:?} ({:?})", to, role), &r);
        }
    }
}

// --- Full happy-path lifecycle ---

#[test]
fn full_lifecycle_happy_path() {
    let chain: Vec<(WorkStatus, WorkStatus, Role)> = vec![
        (WorkStatus::Draft, WorkStatus::Ready, Role::Coordinator),
        (WorkStatus::Ready, WorkStatus::InProgress, Role::Coordinator),
        (WorkStatus::InProgress, WorkStatus::InReview, Role::Implementer),
        (WorkStatus::InReview, WorkStatus::Integrated, Role::Integrator),
        (WorkStatus::Integrated, WorkStatus::Done, Role::Coordinator),
    ];
    for (from, to, role) in &chain {
        let r = from.validate_transition(*to, *role);
        assert_valid(format!("{:?}", from), format!("{:?}", to), &r);
    }
}

// --- Record serde roundtrip ---

#[test]
fn work_serde_all_statuses() {
    for status in &ALL_STATES {
        let mut wi = Work::new("ph-1".into(), "T".into(), "D".into());
        wi.force_status(*status);
        let json = serde_json::to_string(&wi).unwrap();
        let restored: Work = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.status(), *status);
    }
}
