//! Loop runner module - re-exports for backwards compatibility.
//!
//! NOTE: Per domain-types.md, the LoopRunner struct was removed.
//! Loop execution is now handled by `Loop::run()` directly.
//!
//! This module re-exports LoopOutcome from the domain module for
//! backwards compatibility. It will be kept for potential future
//! runner subprocess work (runner-no-net, runner-net, runner-heavy).

// Re-export LoopOutcome from domain for backwards compatibility
pub use crate::domain::LoopOutcome;

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
