//! Ralph Wiggum loops per role. Stage 7 ships the Implementer
//! subset; Stage 8 adds the Reviewer. Researcher / Director land in
//! later stages.

mod action;
mod config;
mod director;
mod dispatch;
mod implementer;
mod lifeguard;
mod parse;
mod reviewer;
pub mod scope;

pub use action::AgentAction;
pub use config::{AgentsConfig, DirectorConfig, ImplementerConfig, ReviewerConfig};
pub use director::{
    ActionFingerprint, DirectorAction, DirectorDeps, DirectorError, DirectorMode, DirectorPatternTracker,
    DirectorStatusMap, DirectorStatusSnapshot, DirectorStore, PatternConfig, PatternObservation, WorkSpawner,
    build_director_state, compute_state_hash, director_accept_bundle, next_mode, parse_director_actions,
    reconcile_director, run_director,
};
pub use dispatch::{ActionResult, DispatchError, RealTools, ToolExecutor, dispatch_action};
pub use implementer::{BundleSink, BundleSinkError, Deps, ImplementerError, run_implementer};
pub use lifeguard::{Decision, Lifeguard, canonical_hash};
pub use parse::{ParseError, parse_actions, parse_one};
pub use reviewer::{
    ParseError as ReviewerParseError, ReviewerDeps, ReviewerError, VERIFICATION_CAP, git_show, parse_verdict,
    read_file_contents, render_issue_summary, run_reviewer, strip_commit_header, truncate_diff,
};
// `BundleUpdateSink` / `BundleUpdateError` were relocated to `store` per
// docs/design/2026-04-22-integrator.md (Phase 1c cross-doc reconciliation).
// Consumers should import from `store` directly: `use store::{BundleUpdateSink,
// BundleUpdateError}`. The Reviewer continues to function at the same trait
// shape under the new module path.
