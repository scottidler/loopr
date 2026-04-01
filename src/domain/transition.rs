/// Result of a validated state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The transition is valid and moves to a new state.
    Changed,
    /// The target state equals the current state (idempotent no-op).
    Unchanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transition_eq() {
        assert_eq!(Transition::Changed, Transition::Changed);
        assert_eq!(Transition::Unchanged, Transition::Unchanged);
        assert_ne!(Transition::Changed, Transition::Unchanged);
    }

    #[test]
    fn test_transition_copy() {
        let t = Transition::Changed;
        let t2 = t;
        assert_eq!(t, t2);
    }
}
