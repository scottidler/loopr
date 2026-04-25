//! `AgentAction`: the five-variant action set the Implementer emits.
//!
//! The five-variant subset is intentional (design doc Alternative 3).
//! v3/v4 shipped 20+ variants; most were dead code at first gate.
//! Reviewer / Director / Research actions land in Stage 8+ when
//! their agents materialize.
//!
//! The wire form is a JSON array at the prompt boundary:
//! ```text
//! [{"type":"run_tool","tool":"bash","input":{"command":"ls"}},
//!  {"type":"propose_bundle","claims":["tests pass"]}]
//! ```
//! `serde(tag = "type", rename_all = "snake_case")` handles the
//! encoding; callers get typed variants at the dispatch boundary.

use serde::{Deserialize, Serialize};

/// One step the Implementer wants the driver to execute. Emitted as
/// part of a JSON array; the driver iterates and dispatches each in
/// order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentAction {
    /// Invoke a tool by name with typed JSON input. The tool
    /// registry validates the input shape; a mismatch surfaces as
    /// a dispatch error and the Implementer's self-correction path
    /// can re-prompt.
    RunTool { tool: String, input: serde_json::Value },

    /// Git add + commit the currently staged work. Uses `git add -A`
    /// so newly created files are included - `git add -u` would
    /// skip untracked files and cause `git commit` to fail with
    /// "nothing added to commit" when the agent writes a new file.
    CommitChanges { message: String },

    /// Finalize the Bundle. The dispatcher records HEAD SHA,
    /// computes `loc_changed` against the worktree base, and
    /// persists the Bundle via `BundlesStore::create`. No
    /// description field - Bundle references Work for spec content.
    ProposeBundle { claims: Vec<String> },

    /// No-op completion. Used when the work turned out to require
    /// no code changes. Emits a Bundle with `noop_reason = Some(...)`
    /// so downstream can distinguish intentional noops from forced
    /// ones.
    Done { message: String },

    /// Escalate to a human. The dispatcher commits any partially-
    /// staged work so a human can inspect, then returns
    /// `Err(EscalationNeeded)` to the coordinator.
    NeedHelp { reason: String },
}

impl AgentAction {
    /// Discriminator string for span fields and log lines. Mirrors the
    /// serde tag (`snake_case`) so an event field's `action_kind` value
    /// matches what appears on the wire.
    pub fn kind(&self) -> &'static str {
        match self {
            AgentAction::RunTool { .. } => "run_tool",
            AgentAction::CommitChanges { .. } => "commit_changes",
            AgentAction::ProposeBundle { .. } => "propose_bundle",
            AgentAction::Done { .. } => "done",
            AgentAction::NeedHelp { .. } => "need_help",
        }
    }
}
