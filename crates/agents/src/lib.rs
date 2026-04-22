//! Ralph Wiggum loops per role. Stage 7 ships the Implementer
//! subset; Stage 8 adds the Reviewer. Researcher / Director land in
//! later stages.

mod action;
mod config;
mod dispatch;
mod implementer;
mod lifeguard;
mod parse;
mod reviewer;

pub use action::AgentAction;
pub use config::{AgentsConfig, ImplementerConfig, ReviewerConfig};
pub use dispatch::{ActionResult, DispatchError, RealTools, ToolExecutor, dispatch_action};
pub use implementer::{BundleSink, BundleSinkError, Deps, ImplementerError, run_implementer};
pub use lifeguard::{Decision, Lifeguard, canonical_hash};
pub use parse::{ParseError, parse_actions, parse_one};
pub use reviewer::{
    BundleUpdateError, BundleUpdateSink, ParseError as ReviewerParseError, ReviewerDeps, ReviewerError,
    VERIFICATION_CAP, git_show, parse_verdict, read_file_contents, render_issue_summary, run_reviewer,
    strip_commit_header, truncate_diff,
};
