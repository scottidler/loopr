//! Git command wrappers. Every git invocation goes through `git_cmd(repo_path)`
//! which sets `.current_dir(repo_path)` and `.env("LC_ALL", "C")`. The
//! `LC_ALL=C` is mandatory (R2 hardening): without it, localized stderr phrases
//! defeat our `SeqTaken` classifier and a retryable "already exists" error
//! would bubble up as a fatal `GitCommand`.
//!
//! Phase 1 ships stubs for `remove_worktree` only (to make `Worktree::Drop`
//! compile). Phase 2 fills in the rest.

use std::path::Path;
use std::process::Command;

use crate::error::WorktreeError;

/// Helper: construct a `Command` for a git subprocess rooted at `repo_path`
/// with `LC_ALL=C` forced so stderr is in the English phrasing our classifiers
/// depend on.
pub(crate) fn git_cmd(repo_path: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_path).env("LC_ALL", "C");
    cmd
}

/// Remove a worktree registered at `path`. `git worktree remove --force`
/// handles dirty worktrees; the forcefulness is deliberate because worktree
/// contents are agent-produced output, not user data.
///
/// Phase 1: best-effort stub. The `git_cmd` call below is wired through so
/// the helper isn't dead-code-flagged; Phase 2 replaces it with the real
/// `.args(["worktree", "remove", "--force", …])` invocation.
pub(crate) fn remove_worktree(repo_path: &Path, path: &Path) -> Result<(), WorktreeError> {
    let _ = git_cmd(repo_path);
    let _ = path;
    Ok(())
}
