//! Idempotent injection of loopr's exclude patterns into `.git/info/exclude`.
//!
//! The amended pattern list (vision.md Target Repo Layout; Phase 8 of
//! `docs/design/2026-06-09-code-review-remediation.md`):
//!   .loopr/worktrees/
//!   .loopr/socket
//!   .loopr/daemon.*       (covers daemon.pid / .version / .process-id / .startup-error)
//!   .loopr/config.yml
//!   .loopr/active-session
//!   .loopr/records/
//!   .loopr/costs.jsonl
//!   .loopr/prompts/
//!
//! `.loopr/taskstore/` is deliberately NOT excluded — per vision, TaskStore
//! IS committed.
//!
//! Injection is **per-pattern append**, not marker-gated all-or-nothing: a
//! `# loopr-managed` marker line is written for readability, but membership
//! is decided line-by-line so a list that GROWS in a later release reaches
//! already-init'd targets (the old marker-gate skipped them, leaving
//! e.g. `.loopr/records/` — multi-MB transcripts — committable forever).
//! Whatever the user already has stays; only genuinely-missing patterns are
//! appended. A read error other than `NotFound` propagates rather than
//! clobbering the user's exclude file.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

use tracing::instrument;

use crate::error::WorktreeError;

const LOOPR_EXCLUDE_MARKER: &str = "# loopr-managed";
const LOOPR_EXCLUDES: &[&str] = &[
    ".loopr/worktrees/",
    ".loopr/socket",
    // Glob covers every daemon sentinel: daemon.pid, daemon.version,
    // daemon.process-id, daemon.startup-error.
    ".loopr/daemon.*",
    ".loopr/config.yml",
    // The per-target active-session pointer is run-local state.
    ".loopr/active-session",
    // `.loopr/records/` holds derived markdown summaries and append-only
    // LLM transcripts. Transcripts can run to multiple MB and may capture
    // redacted-but-still-debug-grade prompt text; never commit.
    ".loopr/records/",
    // `.loopr/costs.jsonl` is the per-run append-only LLM cost ledger
    // (Phase 6 cost audit). Run-local telemetry; never commit.
    ".loopr/costs.jsonl",
    // Prompts are seeded by `loopr init` from the baked tree; per vision
    // they are not committed by default (operators edit in place locally).
    ".loopr/prompts/",
];

#[instrument(name = "worktree.ensure_loopr_excludes", level = "debug", skip_all, fields(repo_path = %repo_path.display()), err)]
pub fn ensure_loopr_excludes(repo_path: &Path) -> Result<(), WorktreeError> {
    let exclude_path = repo_path.join(".git").join("info").join("exclude");
    // A missing exclude file is the fresh-init case (empty). ANY other read
    // error (permissions, I/O) must propagate: silently `unwrap_or_default`
    // here would clobber a present-but-unreadable file with loopr's patterns
    // alone, destroying the user's content.
    let existing = match std::fs::read_to_string(&exclude_path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };

    let present: HashSet<&str> = existing.lines().map(str::trim).collect();
    let marker_present = present.contains(LOOPR_EXCLUDE_MARKER);
    let missing: Vec<&'static str> = LOOPR_EXCLUDES
        .iter()
        .copied()
        .filter(|p| !present.contains(*p))
        .collect();
    drop(present);

    if missing.is_empty() {
        return Ok(());
    }

    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !marker_present {
        content.push_str(LOOPR_EXCLUDE_MARKER);
        content.push('\n');
    }
    for pattern in missing {
        content.push_str(pattern);
        content.push('\n');
    }
    std::fs::write(&exclude_path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests;
