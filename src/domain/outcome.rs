//! Loop execution outcome types.
//!
//! This module defines the result types for loop execution.

/// Outcome of a loop execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopOutcome {
    /// Loop completed successfully - validation passed
    Complete,
    /// Loop failed - max iterations exhausted or unrecoverable error
    Failed(String),
    /// Loop was invalidated by a parent re-iteration
    Invalidated,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_outcome_variants() {
        assert_eq!(LoopOutcome::Complete, LoopOutcome::Complete);
        assert_eq!(LoopOutcome::Failed("test".into()), LoopOutcome::Failed("test".into()));
        assert_eq!(LoopOutcome::Invalidated, LoopOutcome::Invalidated);
        assert_ne!(LoopOutcome::Complete, LoopOutcome::Invalidated);
    }

    #[test]
    fn test_loop_outcome_debug() {
        assert_eq!(format!("{:?}", LoopOutcome::Complete), "Complete");
        assert_eq!(format!("{:?}", LoopOutcome::Failed("error".into())), "Failed(\"error\")");
        assert_eq!(format!("{:?}", LoopOutcome::Invalidated), "Invalidated");
    }

    #[test]
    fn test_loop_outcome_clone() {
        let outcome = LoopOutcome::Failed("test".into());
        let cloned = outcome.clone();
        assert_eq!(outcome, cloned);
    }
}
