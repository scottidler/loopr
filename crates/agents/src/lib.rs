//! Ralph Wiggum loops per role. Stage 7 ships the Implementer
//! subset; Reviewer / Researcher / Director land in later stages.

mod action;
mod dispatch;
mod lifeguard;
mod parse;

pub use action::AgentAction;
pub use dispatch::{ActionResult, DispatchError, ToolExecutor, dispatch_action};
pub use lifeguard::{Lifeguard, Verdict, canonical_hash};
pub use parse::{ParseError, parse_actions, parse_one};
