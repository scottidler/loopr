//! Loop runner module - implements Ralph Wiggum iteration pattern.
//!
//! This module provides the core loop execution logic, including:
//! - LoopRunner for executing single loops
//! - LoopOutcome for representing execution results
//! - Fresh context pattern with accumulated feedback

mod loop_runner;

pub use loop_runner::{LoopOutcome, LoopRunner, LoopRunnerConfig};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_exports() {
        // Verify module exports are accessible
        let _outcome = LoopOutcome::Complete;
        assert!(matches!(_outcome, LoopOutcome::Complete));
    }
}
