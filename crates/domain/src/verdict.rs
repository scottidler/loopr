//! `Verdict`: the Reviewer agent's typed return value.
//!
//! Three-way outcome (`Accept` / `ChangeRequested` / `Reject`) with
//! structured per-issue detail when changes are requested. The Verdict
//! flows through the Reviewer's return value; its outcome is also
//! encoded in `Bundle.status` (`Reviewed` for Accept, `Rejected` for
//! the other two) and a text summary on `Bundle.verification`, so the
//! downstream daemon can route on both the typed Verdict and the
//! persisted Bundle state.
//!
//! Not a `Record`: no JSONL persistence, no SQLite index, no FSM. Pure
//! data carrier. The "list verdicts for bundle X" query is answered by
//! reading Bundle state plus verification text; denormalizing into a
//! `verdicts.jsonl` would double the write per review without adding a
//! query the Bundle can't already answer. Revisit when a real run
//! surfaces a query this decision can't serve.

use serde::{Deserialize, Serialize};

/// Reviewer outcome. `ChangeRequested` carries both a one-liner
/// `summary` (written into `Bundle.verification` for quick scanning)
/// and structured `reasons` (the payload a future Implementer retry
/// prompt consumes issue-by-issue). Keeping both in the Verdict means
/// downstream routing never reconstructs structure from a rendered
/// string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Verdict {
    Accept { summary: String },
    ChangeRequested { summary: String, reasons: Vec<ReviewIssue> },
    Reject { reason: String },
}

/// One structured issue inside a `ChangeRequested` verdict. `file` and
/// `line` locate the concern; `message` describes it; `suggestion`
/// optionally proposes a fix. Severity is independent of the
/// enclosing verdict kind: an `Accept` can carry non-blocking `Info`
/// notes in a future shape, and a `ChangeRequested` can mix `Error`,
/// `Warning`, and `Info` reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewIssue {
    pub severity: Severity,
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub message: String,
    #[serde(default)]
    pub suggestion: Option<String>,
}

/// Issue severity. Blocking issues force `change_requested`;
/// non-blocking issues are advisory and may accompany any verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[cfg(test)]
mod tests;
