//! `FailureReason`: the typed failure enum carried on `Work` and
//! `Bundle` so a terminal-or-Blocked record records *why* it failed in
//! a machine-matchable form, not only a free-text string.
//!
//! The vision (vision.md "Failure posture") specifies this enum; until
//! Phase 4 of the 2026-06-09 code-review remediation it was vapor (no
//! definition, no `catch_unwind`, no persisted field). The companion
//! free-text detail lives in the record's existing field
//! (`Work.blocked_reason`, `Bundle.verification`); `FailureReason` is
//! the discriminant a consumer branches on without parsing prose.
//!
//! Serde: externally-tagged, kebab-case variant tags. The carrying
//! field on `Work`/`Bundle` is snake_case (matching `blocked_reason`,
//! `session_failure_count`), so a JSONL row reads
//! `"failure_reason":"panic"` or
//! `"failure_reason":{"tool-failure":{"tool":"bash"}}`. The field is
//! `#[serde(default)]` `Option` on both records, so old rows written
//! before this enum existed deserialize clean as `None`.

use serde::{Deserialize, Serialize};

/// Why a `Work` or `Bundle` reached a Blocked or terminal failure
/// state. `None` on the record means "no failure recorded" (the
/// success path, or a record that has not failed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureReason {
    /// The role's LLM call(s) exhausted the configured token budget.
    TokenBudget,
    /// A tool invocation failed unrecoverably; `tool` names which one.
    ToolFailure { tool: String },
    /// The Reviewer rejected the Bundle on merit.
    ReviewerRejection,
    /// Acceptance criteria were not met (reviewer or self-check).
    AcUnmet,
    /// The role's future panicked; `catch_unwind` recorded it before
    /// the worktree-cleanup tail ran.
    Panic,
    /// A daemon crash interrupted the role mid-flight; surfaced by the
    /// startup reconcile sweep against a non-terminal carried-forward
    /// worktree.
    CrashInterrupted,
    /// Anything not covered above; carries the detail inline.
    Other(String),
}

#[cfg(test)]
mod tests;
