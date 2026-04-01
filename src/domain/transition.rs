use std::fmt::Debug;

use crate::error::LooprError;

use super::role::Role;

/// Result of a validated state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The transition is valid and moves to a new state.
    Changed,
    /// The target state equals the current state (idempotent no-op).
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct TransitionRule<S> {
    pub from: S,
    pub to: S,
    pub role: Option<Role>,
}

pub fn validate_transition<S: PartialEq + Copy + Debug>(
    current: S,
    target: S,
    role: Role,
    rules: &[TransitionRule<S>],
) -> crate::error::Result<()> {
    // Idempotent: requesting the current state is always a no-op success.
    if current == target {
        return Ok(());
    }
    let allowed = rules
        .iter()
        .any(|r| r.from == current && r.to == target && r.role.is_none_or(|required| required == role));
    if !allowed {
        return Err(LooprError::InvalidTransition {
            from: format!("{:?}", current),
            to: format!("{:?}", target),
            role: role.to_string(),
        });
    }
    Ok(())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum TestState {
        A,
        B,
        C,
    }

    fn test_rules() -> Vec<TransitionRule<TestState>> {
        vec![
            TransitionRule {
                from: TestState::A,
                to: TestState::B,
                role: Some(Role::Coordinator),
            },
            TransitionRule {
                from: TestState::B,
                to: TestState::C,
                role: None,
            },
            TransitionRule {
                from: TestState::A,
                to: TestState::C,
                role: Some(Role::Implementer),
            },
        ]
    }

    #[test]
    fn test_valid_transition_with_role() {
        let rules = test_rules();
        let result = validate_transition(TestState::A, TestState::B, Role::Coordinator, &rules);
        assert!(result.is_ok());
    }

    #[test]
    fn test_valid_transition_any_role() {
        let rules = test_rules();
        // B->C has role: None, so any role should work
        for role in [Role::Coordinator, Role::Integrator, Role::Implementer] {
            let result = validate_transition(TestState::B, TestState::C, role, &rules);
            assert!(result.is_ok(), "Expected B->C to succeed for {:?}", role);
        }
    }

    #[test]
    fn test_invalid_transition_wrong_role() {
        let rules = test_rules();
        let result = validate_transition(TestState::A, TestState::B, Role::Implementer, &rules);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, LooprError::InvalidTransition { .. }));
    }

    #[test]
    fn test_invalid_transition_no_rule() {
        let rules = test_rules();
        let result = validate_transition(TestState::C, TestState::A, Role::Coordinator, &rules);
        assert!(result.is_err());
    }

    #[test]
    fn test_idempotent_transition_always_ok() {
        let rules = test_rules();
        // Same state -> same state is always OK regardless of role
        for role in [Role::Coordinator, Role::Integrator, Role::Implementer, Role::Reviewer] {
            for state in [TestState::A, TestState::B, TestState::C] {
                let result = validate_transition(state, state, role, &rules);
                assert!(result.is_ok(), "Expected {:?}->{:?} to succeed for {:?}", state, state, role);
            }
        }
    }

    #[test]
    fn test_invalid_transition_error_message() {
        let rules = test_rules();
        let result = validate_transition(TestState::C, TestState::A, Role::Coordinator, &rules);
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("C"), "Error should mention source state");
        assert!(msg.contains("A"), "Error should mention target state");
        assert!(msg.contains("coordinator"), "Error should mention role");
    }
}
