//! Ralph Wiggum loops per role. Stage 7 ships the Implementer
//! subset; Reviewer / Researcher / Director land in later stages.

mod action;
mod config;
mod dispatch;
mod implementer;
mod lifeguard;
mod parse;

pub use action::AgentAction;
pub use config::ImplementerConfig;
pub use dispatch::{ActionResult, DispatchError, RealTools, ToolExecutor, dispatch_action};
pub use implementer::{BundleSink, BundleSinkError, Deps, ImplementerError, run_implementer};
pub use lifeguard::{Lifeguard, Verdict, canonical_hash};
pub use parse::{ParseError, parse_actions, parse_one};
