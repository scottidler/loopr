//! Strongly-typed IPC method vocabulary for the daemon dispatch layer.
//!
//! The wire protocol still carries method names as strings (`DaemonRequest.method: String`),
//! because clients in other languages/versions need a stable string contract. Internally,
//! though, we parse once at the dispatch boundary into `DaemonMethod` and match exhaustively
//! against it. That gives three guarantees the old naked `match req.method.as_str() { ... }`
//! could not:
//!
//! 1. **Exhaustiveness.** Adding a new variant without handling it fails compilation rather
//!    than silently routing to the `method_not_found` arm.
//! 2. **Typo-proof routing.** `DaemonMethod::PlanCreate` is checked; `"plan.creaate"` is not.
//! 3. **Coverage tests.** The enum can be iterated (via `strum::IntoEnumIterator`), so a
//!    single test can assert that every method round-trips through the string form without
//!    manually enumerating 70+ names.
//!
//! If you add a new IPC method:
//! - Add a variant here with the correct `#[strum(serialize = "...")]`.
//! - Add a match arm in `src/daemon/handlers.rs::dispatch`.
//! - The coverage test in that module's `tests` will enforce both.

use strum::{Display, EnumIter, EnumString, IntoStaticStr};

/// Every IPC method the daemon accepts.
///
/// `strum(serialize = "...")` declares the exact wire name; `from_str` parses a string into
/// the matching variant (returning `strum::ParseError::VariantNotFound` on miss), and
/// `Display` / `IntoStaticStr` let us round-trip back to the string form for logs and
/// error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, EnumIter, IntoStaticStr)]
pub enum DaemonMethod {
    // system.*
    #[strum(serialize = "system.handshake")]
    SystemHandshake,
    #[strum(serialize = "system.init")]
    SystemInit,
    #[strum(serialize = "system.status")]
    SystemStatus,
    #[strum(serialize = "system.shutdown")]
    SystemShutdown,
    #[strum(serialize = "system.recover")]
    SystemRecover,

    // plan.*
    #[strum(serialize = "plan.create")]
    PlanCreate,
    #[strum(serialize = "plan.get")]
    PlanGet,
    #[strum(serialize = "plan.list")]
    PlanList,
    #[strum(serialize = "plan.transition")]
    PlanTransition,
    #[strum(serialize = "plan.update")]
    PlanUpdate,

    // spec.*
    #[strum(serialize = "spec.create")]
    SpecCreate,
    #[strum(serialize = "spec.get")]
    SpecGet,
    #[strum(serialize = "spec.list")]
    SpecList,
    #[strum(serialize = "spec.transition")]
    SpecTransition,
    #[strum(serialize = "spec.update")]
    SpecUpdate,

    // phase.*
    #[strum(serialize = "phase.create")]
    PhaseCreate,
    #[strum(serialize = "phase.get")]
    PhaseGet,
    #[strum(serialize = "phase.list")]
    PhaseList,
    #[strum(serialize = "phase.transition")]
    PhaseTransition,
    #[strum(serialize = "phase.update")]
    PhaseUpdate,

    // work.*
    #[strum(serialize = "work.create")]
    WorkCreate,
    #[strum(serialize = "work.get")]
    WorkGet,
    #[strum(serialize = "work.list")]
    WorkList,
    #[strum(serialize = "work.transition")]
    WorkTransition,
    #[strum(serialize = "work.update")]
    WorkUpdate,

    // bundle.*
    #[strum(serialize = "bundle.create")]
    BundleCreate,
    #[strum(serialize = "bundle.get")]
    BundleGet,
    #[strum(serialize = "bundle.list")]
    BundleList,
    #[strum(serialize = "bundle.transition")]
    BundleTransition,
    #[strum(serialize = "bundle.update")]
    BundleUpdate,

    // tick.*
    #[strum(serialize = "tick.create")]
    TickCreate,
    #[strum(serialize = "tick.get")]
    TickGet,
    #[strum(serialize = "tick.list")]
    TickList,
    #[strum(serialize = "tick.transition")]
    TickTransition,
    #[strum(serialize = "tick.update")]
    TickUpdate,

    // learning.*
    #[strum(serialize = "learning.create")]
    LearningCreate,
    #[strum(serialize = "learning.get")]
    LearningGet,
    #[strum(serialize = "learning.list")]
    LearningList,
    #[strum(serialize = "learning.update")]
    LearningUpdate,
    #[strum(serialize = "learning.reinforce")]
    LearningReinforce,
    #[strum(serialize = "learning.contradict")]
    LearningContradict,
    #[strum(serialize = "learning.promote")]
    LearningPromote,
    #[strum(serialize = "learning.demote")]
    LearningDemote,

    // lock.*
    #[strum(serialize = "lock.create")]
    LockCreate,
    #[strum(serialize = "lock.get")]
    LockGet,
    #[strum(serialize = "lock.list")]
    LockList,
    #[strum(serialize = "lock.release")]
    LockRelease,
    #[strum(serialize = "lock.expire")]
    LockExpire,

    // worktree.*
    #[strum(serialize = "worktree.create")]
    WorktreeCreate,
    #[strum(serialize = "worktree.list")]
    WorktreeList,
    #[strum(serialize = "worktree.cleanup")]
    WorktreeCleanup,
    #[strum(serialize = "worktree.refresh")]
    WorktreeRefresh,

    // integrator.*
    #[strum(serialize = "integrator.validate")]
    IntegratorValidate,
    #[strum(serialize = "integrator.publish")]
    IntegratorPublish,

    // validator.*
    #[strum(serialize = "validator.validate")]
    ValidatorValidate,
    #[strum(serialize = "validator.report")]
    ValidatorReport,
    #[strum(serialize = "validator.reports")]
    ValidatorReports,

    // coverage.*
    #[strum(serialize = "coverage.evaluate")]
    CoverageEvaluate,

    // tool.* / tools.*
    #[strum(serialize = "tool.list")]
    ToolList,
    #[strum(serialize = "tools.register")]
    ToolsRegister,

    // doc.*
    #[strum(serialize = "doc.accept")]
    DocAccept,
    #[strum(serialize = "doc.inject")]
    DocInject,

    // chat.*
    #[strum(serialize = "chat.submit")]
    ChatSubmit,
    #[strum(serialize = "chat.attach")]
    ChatAttach,
    #[strum(serialize = "chat.history")]
    ChatHistory,

    // agent.*
    #[strum(serialize = "agent.start")]
    AgentStart,
    #[strum(serialize = "agent.stop")]
    AgentStop,
    #[strum(serialize = "agent.pause")]
    AgentPause,
    #[strum(serialize = "agent.resume")]
    AgentResume,
    #[strum(serialize = "agent.status")]
    AgentStatus,
    #[strum(serialize = "agent.list")]
    AgentList,
    #[strum(serialize = "agent.output")]
    AgentOutput,

    // director.*
    #[strum(serialize = "director.start_plan_intake")]
    DirectorStartPlanIntake,
    #[strum(serialize = "director.user_message")]
    DirectorUserMessage,

    // decomposer.*
    #[strum(serialize = "decomposer.decompose")]
    DecomposerDecompose,
    #[strum(serialize = "decomposer.ratify")]
    DecomposerRatify,
    #[strum(serialize = "decomposer.abandon_children")]
    DecomposerAbandonChildren,
    #[strum(serialize = "decomposer.re_decompose")]
    DecomposerReDecompose,
    #[strum(serialize = "decomposer.handle_failure")]
    DecomposerHandleFailure,
}

impl DaemonMethod {
    /// The wire-format string for this method (what clients send in `DaemonRequest.method`).
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use strum::IntoEnumIterator;

    #[test]
    fn every_method_round_trips_through_string_form() {
        // For every variant, the wire string must parse back to the same variant.
        // Catches typos in the `#[strum(serialize = "...")]` attributes.
        for method in DaemonMethod::iter() {
            let s: &str = method.into();
            let parsed = DaemonMethod::from_str(s).unwrap_or_else(|e| panic!("{} failed to round-trip: {}", s, e));
            assert_eq!(parsed, method, "variant {:?} did not round-trip to itself", method);
        }
    }

    #[test]
    fn unknown_method_string_returns_parse_error() {
        assert!(DaemonMethod::from_str("plan.totally-made-up").is_err());
        assert!(DaemonMethod::from_str("").is_err());
        assert!(DaemonMethod::from_str("plan.creaate").is_err());
    }

    #[test]
    fn method_count_matches_expected_surface() {
        // Exact-count guard. Update this number whenever you add or remove a method
        // in DaemonMethod, and keep the dispatch match in src/daemon/handlers.rs in sync.
        let count = DaemonMethod::iter().count();
        assert_eq!(count, 79, "DaemonMethod should have exactly 79 variants; got {}", count);
    }
}
