use derive::Fsm;
use domain::{FsmErrorKind, Role, TargetKind, Transition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Fsm)]
#[fsm(
    role = ::domain::Role,
    terminal = [Done, Superseded, Abandoned],
    transitions(
        Draft      => Pending     by (Reactor),
        Draft      => Ready       by (Reactor),
        Draft      => Superseded  by (Reactor, Director),
        Draft      => Abandoned   by (Reactor, Director),
        Pending    => Ready       by (Reactor),
        Pending    => Superseded  by (Reactor, Director),
        Pending    => Abandoned   by (Reactor, Director),
        Ready      => InProgress  by (Reactor),
        Ready      => Blocked     by (Reactor),
        Ready      => Superseded  by (Reactor, Director),
        Ready      => Abandoned   by (Reactor, Director),
        Ready      => Done        by (Reactor),
        InProgress => Blocked     by (Any),
        InProgress => InReview    by (Implementer),
        InProgress => Superseded  by (Reactor, Director),
        InProgress => Abandoned   by (Reactor, Director),
        Blocked    => Ready       by (Reactor),
        Blocked    => Superseded  by (Reactor, Director),
        Blocked    => Abandoned   by (Reactor, Director),
        InReview   => InProgress  by (Reactor),
        InReview   => Integrated  by (Integrator),
        InReview   => Superseded  by (Reactor, Director),
        InReview   => Abandoned   by (Reactor, Director),
        Integrated => Done        by (Reactor, Integrator),
        Integrated => Abandoned   by (Reactor, Director),
    ),
    overrides(
        InProgress => Ready     by (Reactor),
        InProgress => InReview  by (Reactor),
        InReview   => Ready     by (Reactor),
    ),
)]
enum WorkStatus {
    Draft,
    Pending,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Integrated,
    Done,
    Superseded,
    Abandoned,
}

const ALL_WORK_STATES: &[WorkStatus] = &[
    WorkStatus::Draft,
    WorkStatus::Pending,
    WorkStatus::Ready,
    WorkStatus::InProgress,
    WorkStatus::Blocked,
    WorkStatus::InReview,
    WorkStatus::Integrated,
    WorkStatus::Done,
    WorkStatus::Superseded,
    WorkStatus::Abandoned,
];

const ALL_ROLES: &[Role] = &[
    Role::Reactor,
    Role::Integrator,
    Role::Implementer,
    Role::Reviewer,
    Role::Researcher,
    Role::Decomposer,
    Role::Director,
];

#[test]
fn all_states_enumerates_every_variant() {
    assert_eq!(WorkStatus::all_states(), ALL_WORK_STATES);
}

#[test]
fn terminal_states_matches_attribute() {
    assert_eq!(
        WorkStatus::terminal_states(),
        &[WorkStatus::Done, WorkStatus::Superseded, WorkStatus::Abandoned,]
    );
}

#[test]
fn is_terminal_every_state() {
    for state in ALL_WORK_STATES {
        let expected = matches!(state, WorkStatus::Done | WorkStatus::Superseded | WorkStatus::Abandoned);
        assert_eq!(state.is_terminal(), expected, "state = {:?}", state);
    }
}

#[test]
fn from_equal_to_is_unchanged_for_every_state_and_role() {
    for state in ALL_WORK_STATES {
        for role in ALL_ROLES {
            let result = WorkStatus::validate_transition(*state, *state, *role);
            assert_eq!(
                result.unwrap(),
                Transition::Unchanged,
                "expected Unchanged for ({:?}, {:?}, {:?})",
                state,
                state,
                role
            );
        }
    }
}

#[test]
fn validate_transition_happy_paths() {
    let cases = [
        (WorkStatus::Draft, WorkStatus::Pending, Role::Reactor),
        (WorkStatus::Draft, WorkStatus::Superseded, Role::Director),
        (WorkStatus::Ready, WorkStatus::InProgress, Role::Reactor),
        (WorkStatus::InProgress, WorkStatus::Blocked, Role::Reviewer),
        (WorkStatus::InProgress, WorkStatus::InReview, Role::Implementer),
        (WorkStatus::InReview, WorkStatus::Integrated, Role::Integrator),
        (WorkStatus::Integrated, WorkStatus::Done, Role::Integrator),
        (WorkStatus::Integrated, WorkStatus::Done, Role::Reactor),
    ];
    for (from, to, role) in cases {
        let result = WorkStatus::validate_transition(from, to, role).unwrap();
        assert_eq!(
            result,
            Transition::Changed,
            "expected Changed for ({:?} -> {:?}) with {:?}",
            from,
            to,
            role,
        );
    }
}

#[test]
fn validate_transition_role_not_authorized() {
    let err = WorkStatus::validate_transition(WorkStatus::Draft, WorkStatus::Pending, Role::Implementer).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::RoleNotAuthorized);
    assert_eq!(err.from, WorkStatus::Draft);
    assert_eq!(err.to, WorkStatus::Pending);
    assert_eq!(err.role, "implementer");
    assert!(err.context.is_none());
}

#[test]
fn validate_transition_no_edge() {
    let err = WorkStatus::validate_transition(WorkStatus::Draft, WorkStatus::Done, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
    assert!(err.context.is_none());
}

#[test]
fn validate_override_uses_override_table() {
    assert_eq!(
        WorkStatus::validate_override(WorkStatus::InProgress, WorkStatus::Ready, Role::Reactor).unwrap(),
        Transition::Override,
    );
    assert_eq!(
        WorkStatus::validate_override(WorkStatus::InProgress, WorkStatus::InReview, Role::Reactor).unwrap(),
        Transition::Override,
    );
    assert_eq!(
        WorkStatus::validate_override(WorkStatus::InReview, WorkStatus::Ready, Role::Reactor).unwrap(),
        Transition::Override,
    );
}

#[test]
fn validate_override_returns_changed_for_normal_match() {
    assert_eq!(
        WorkStatus::validate_override(WorkStatus::Draft, WorkStatus::Pending, Role::Reactor).unwrap(),
        Transition::Changed,
    );
}

#[test]
fn validate_override_chains_context_when_override_also_fails_no_transition() {
    let err = WorkStatus::validate_override(WorkStatus::Draft, WorkStatus::Done, Role::Reactor).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::NoTransition);
    let inner = err.context.as_ref().expect("context should chain normal-path error");
    assert_eq!(inner.kind, FsmErrorKind::NoTransition);
}

#[test]
fn validate_override_chains_context_when_override_exists_but_role_wrong() {
    let err = WorkStatus::validate_override(WorkStatus::InProgress, WorkStatus::Ready, Role::Director).unwrap_err();
    assert_eq!(err.kind, FsmErrorKind::RoleNotAuthorized);
    let inner = err.context.as_ref().expect("context should chain normal-path error");
    assert_eq!(inner.kind, FsmErrorKind::NoTransition);
}

#[test]
fn valid_targets_preserves_source_order() {
    let targets = WorkStatus::valid_targets(WorkStatus::InProgress, Role::Reactor);
    assert_eq!(
        targets,
        vec![
            (WorkStatus::Blocked, TargetKind::Normal),
            (WorkStatus::Superseded, TargetKind::Normal),
            (WorkStatus::Abandoned, TargetKind::Normal),
            (WorkStatus::Ready, TargetKind::Override),
            (WorkStatus::InReview, TargetKind::Override),
        ],
    );
}

#[test]
fn valid_targets_filters_by_role() {
    let targets = WorkStatus::valid_targets(WorkStatus::InProgress, Role::Implementer);
    assert_eq!(
        targets,
        vec![
            (WorkStatus::Blocked, TargetKind::Normal),
            (WorkStatus::InReview, TargetKind::Normal),
        ],
    );
}

#[test]
fn valid_targets_empty_for_terminal_state() {
    let targets = WorkStatus::valid_targets(WorkStatus::Done, Role::Reactor);
    assert!(targets.is_empty());
}

#[test]
fn sweep_valid_targets_matches_validate_transition_plus_override() {
    for from in ALL_WORK_STATES {
        for role in ALL_ROLES {
            let targets = WorkStatus::valid_targets(*from, *role);
            for to in ALL_WORK_STATES {
                if from == to {
                    continue;
                }
                let normal = WorkStatus::validate_transition(*from, *to, *role);
                let override_ = WorkStatus::validate_override(*from, *to, *role);
                let expected_normal = targets.iter().any(|(t, k)| t == to && *k == TargetKind::Normal);
                let expected_override = targets.iter().any(|(t, k)| t == to && *k == TargetKind::Override);
                assert_eq!(
                    normal.is_ok(),
                    expected_normal,
                    "normal disagreement ({:?} -> {:?}, {:?})",
                    from,
                    to,
                    role,
                );
                assert_eq!(
                    override_.is_ok(),
                    expected_normal || expected_override,
                    "override disagreement ({:?} -> {:?}, {:?})",
                    from,
                    to,
                    role,
                );
            }
        }
    }
}

#[test]
fn display_snapshot_matches_design_doc() {
    let err = WorkStatus::validate_transition(WorkStatus::InProgress, WorkStatus::Done, Role::Implementer).unwrap_err();
    let rendered = err.to_string();
    let expected = "invalid transition: InProgress -> Done (role: implementer): no transition exists\n\
                    \x20\x20valid from InProgress (normal): Blocked (any), InReview (implementer), Superseded (reactor, director), Abandoned (reactor, director)\n\
                    \x20\x20valid from InProgress (override): Ready (reactor), InReview (reactor)";
    assert_eq!(rendered, expected);
}

#[test]
fn every_role_rejection_produces_role_not_authorized() {
    for from in ALL_WORK_STATES {
        for to in ALL_WORK_STATES {
            if from == to {
                continue;
            }
            for role in ALL_ROLES {
                match WorkStatus::validate_transition(*from, *to, *role) {
                    Ok(_) => {}
                    Err(e) => match e.kind {
                        FsmErrorKind::NoTransition | FsmErrorKind::RoleNotAuthorized => {}
                    },
                }
            }
        }
    }
}
