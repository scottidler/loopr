//! Idempotent injection of loopr's exclude patterns into `.git/info/exclude`.
//!
//! The design's amended pattern list (D13; vision.md Target Repo Layout):
//!   .loopr/runs/
//!   .loopr/worktrees/
//!   .loopr/socket
//!   .loopr/daemon.pid
//!   .loopr/config.yml
//!
//! `.loopr/taskstore/` is deliberately NOT excluded — per vision, TaskStore
//! IS committed. The `# loopr-managed` marker lets re-runs of this function
//! detect a prior injection and skip; the whole block is append-only to
//! preserve any user edits elsewhere in the file.

use std::path::Path;

use crate::error::WorktreeError;

const LOOPR_EXCLUDE_MARKER: &str = "# loopr-managed";
const LOOPR_EXCLUDES: &[&str] = &[
    ".loopr/runs/",
    ".loopr/worktrees/",
    ".loopr/socket",
    ".loopr/daemon.pid",
    ".loopr/config.yml",
];

pub fn ensure_loopr_excludes(repo_path: &Path) -> Result<(), WorktreeError> {
    let exclude_path = repo_path.join(".git").join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.contains(LOOPR_EXCLUDE_MARKER) {
        return Ok(());
    }

    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut content = existing;
    if !content.ends_with('\n') && !content.is_empty() {
        content.push('\n');
    }
    content.push_str(LOOPR_EXCLUDE_MARKER);
    content.push('\n');
    for pattern in LOOPR_EXCLUDES {
        content.push_str(pattern);
        content.push('\n');
    }
    std::fs::write(&exclude_path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests;
