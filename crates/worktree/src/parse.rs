//! Porcelain output + branch-name parsers.
//!
//! `porcelain` turns `git worktree list --porcelain` into a typed
//! `Vec<WorktreeInfo>`, filtered to entries under our managed root so that
//! user-created worktrees never show up in reconcile. Detached-HEAD
//! entries are dropped (ours always carry a `loopr/wk-*` branch).
//!
//! `branch` is a strict parser for `loopr/wk-<work-id>-<seq>`. We split on
//! the **last** `-` so work_ids that contain `-` (e.g. `wk-abc12`) are
//! handled correctly. Anything that doesn't match the full shape returns
//! `None`; callers treat that as "not ours" and skip without mutating.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use domain::WorkId;

use crate::info::WorktreeInfo;

/// v5 provenance prefix. Every loopr-created branch starts with this.
pub(crate) const BRANCH_PREFIX: &str = "loopr/wk-";

/// Parse `git worktree list --porcelain` output, filtered to paths under
/// `worktree_root`. Detached entries (no `branch` line) are dropped.
pub(crate) fn porcelain(output: &str, worktree_root: &Path) -> Vec<WorktreeInfo> {
    let mut result = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_head = String::new();
    let mut current_branch = String::new();

    for line in output.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            flush(
                &mut result,
                current_path.take(),
                std::mem::take(&mut current_branch),
                std::mem::take(&mut current_head),
                worktree_root,
            );
            current_path = Some(PathBuf::from(path_str));
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            current_head = head.to_string();
        } else if let Some(branch) = line.strip_prefix("branch ") {
            current_branch = branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string();
        }
    }

    flush(&mut result, current_path, current_branch, current_head, worktree_root);
    result
}

fn flush(result: &mut Vec<WorktreeInfo>, path: Option<PathBuf>, branch: String, head: String, worktree_root: &Path) {
    let Some(path) = path else { return };
    if !path.starts_with(worktree_root) {
        return;
    }
    if branch.is_empty() {
        return; // detached HEAD — not ours
    }
    result.push(WorktreeInfo { path, branch, head });
}

/// Parse `loopr/wk-<work-id>-<seq>` → `(WorkId, seq)`. Returns `None` on
/// any shape deviation (missing prefix, missing seq, non-numeric seq, zero
/// seq, empty work-id). Split on the last `-` so work_ids containing `-`
/// work correctly.
pub(crate) fn branch(b: &str) -> Option<(WorkId, u32)> {
    let rest = b.strip_prefix(BRANCH_PREFIX)?;
    let split = rest.rsplit_once('-')?;
    let (work_id_str, seq_str) = split;
    if work_id_str.is_empty() {
        return None;
    }
    let seq: u32 = seq_str.parse().ok()?;
    if seq == 0 {
        return None;
    }
    let work_id = WorkId::from_str(work_id_str).ok()?;
    Some((work_id, seq))
}

#[cfg(test)]
mod tests;
