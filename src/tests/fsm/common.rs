// Common test utilities for FSM tests (interpreter-based).
use crate::domain::role::Role;
use crate::domain::transition::Transition;
use crate::fsm::runtime::FsmInterpreter;
use crate::fsm::status::FsmStatus;

/// All roles for exhaustive wrong-role testing.
pub(super) const ALL_ROLES: [Role; 5] = [
    Role::Coordinator,
    Role::Integrator,
    Role::Implementer,
    Role::Reviewer,
    Role::Researcher,
];

/// Build a shared interpreter from the embedded YAML definitions.
pub(super) fn new_interp() -> FsmInterpreter {
    FsmInterpreter::embedded().expect("embedded FSM definitions must be valid")
}

/// Validate a transition via the interpreter using FsmStatus name mapping.
pub(super) fn vt<S: FsmStatus>(interp: &FsmInterpreter, from: S, to: S, role: Role) -> eyre::Result<Transition> {
    interp.validate_transition(S::fsm_name(), from.to_yaml_name(), to.to_yaml_name(), &role.to_string())
}

/// Validate an override transition via the interpreter.
pub(super) fn vo<S: FsmStatus>(interp: &FsmInterpreter, from: S, to: S, role: Role) -> eyre::Result<Transition> {
    interp.validate_override(S::fsm_name(), from.to_yaml_name(), to.to_yaml_name(), &role.to_string())
}

pub(super) fn assert_valid(
    from: impl std::fmt::Display,
    to: impl std::fmt::Display,
    result: &eyre::Result<Transition>,
) {
    assert!(
        result.is_ok(),
        "{} -> {} should be valid but got: {:?}",
        from,
        to,
        result
    );
}

pub(super) fn assert_invalid(
    from: impl std::fmt::Display,
    to: impl std::fmt::Display,
    result: &eyre::Result<Transition>,
) {
    assert!(result.is_err(), "{} -> {} should be INVALID but succeeded", from, to);
}
