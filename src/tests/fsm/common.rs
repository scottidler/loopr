use crate::domain::role::Role;
use crate::domain::transition::Transition;

// ========================================================================
// Helper: all roles for exhaustive wrong-role testing
// ========================================================================

pub(super) const ALL_ROLES: [Role; 5] = [
    Role::Coordinator,
    Role::Integrator,
    Role::Implementer,
    Role::Reviewer,
    Role::Researcher,
];

pub(super) fn assert_valid(from: impl Into<String>, to: impl Into<String>, result: &crate::error::Result<Transition>) {
    let f = from.into();
    let t = to.into();
    assert!(result.is_ok(), "{} -> {} should be valid but got: {:?}", f, t, result);
}

pub(super) fn assert_invalid(
    from: impl Into<String>,
    to: impl Into<String>,
    result: &crate::error::Result<Transition>,
) {
    let f = from.into();
    let t = to.into();
    assert!(result.is_err(), "{} -> {} should be INVALID but succeeded", f, t);
}
