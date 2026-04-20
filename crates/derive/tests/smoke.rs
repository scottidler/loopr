use derive::Fsm;
use domain::{Role, TargetKind, Transition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Fsm)]
#[fsm(
    role = ::domain::Role,
    terminal = [Done],
    transitions(
        Draft => Ready by (Coordinator),
        Ready => Done by (Coordinator),
    ),
    overrides(
        Ready => Draft by (Coordinator),
    ),
)]
enum MiniStatus {
    Draft,
    Ready,
    Done,
}

#[test]
fn all_states_lists_every_variant() {
    let states = MiniStatus::all_states();
    assert_eq!(states.len(), 3);
    assert!(states.contains(&MiniStatus::Draft));
    assert!(states.contains(&MiniStatus::Ready));
    assert!(states.contains(&MiniStatus::Done));
}

#[test]
fn terminal_states_lists_only_terminals() {
    assert_eq!(MiniStatus::terminal_states(), &[MiniStatus::Done]);
}

#[test]
fn is_terminal_detects_terminal_states() {
    assert!(!MiniStatus::Draft.is_terminal());
    assert!(!MiniStatus::Ready.is_terminal());
    assert!(MiniStatus::Done.is_terminal());
}

#[test]
fn validate_transition_happy_path() {
    let result = MiniStatus::validate_transition(MiniStatus::Draft, MiniStatus::Ready, Role::Coordinator);
    assert_eq!(result.unwrap(), Transition::Changed);
}

#[test]
fn validate_transition_same_state_is_unchanged() {
    let result = MiniStatus::validate_transition(MiniStatus::Draft, MiniStatus::Draft, Role::Coordinator);
    assert_eq!(result.unwrap(), Transition::Unchanged);
}

#[test]
fn validate_transition_rejects_unauthorized_role() {
    let err = MiniStatus::validate_transition(MiniStatus::Draft, MiniStatus::Ready, Role::Director).unwrap_err();
    assert_eq!(err.kind, domain::FsmErrorKind::RoleNotAuthorized);
}

#[test]
fn validate_transition_rejects_missing_edge() {
    let err = MiniStatus::validate_transition(MiniStatus::Draft, MiniStatus::Done, Role::Coordinator).unwrap_err();
    assert_eq!(err.kind, domain::FsmErrorKind::NoTransition);
}

#[test]
fn validate_override_falls_back_to_override_table() {
    let result = MiniStatus::validate_override(MiniStatus::Ready, MiniStatus::Draft, Role::Coordinator);
    assert_eq!(result.unwrap(), Transition::Override);
}

#[test]
fn valid_targets_includes_normal_and_override() {
    let targets = MiniStatus::valid_targets(MiniStatus::Ready, Role::Coordinator);
    assert!(targets.contains(&(MiniStatus::Done, TargetKind::Normal)));
    assert!(targets.contains(&(MiniStatus::Draft, TargetKind::Override)));
}
