//! Per-attempt git worktree lifecycle. Infrastructure-only.
//!
//! Exposes the `Worktree` RAII handle, the `AttemptCleanupPolicy` enum + its
//! `WorktreeConfig` wrapper, and a set of free functions consumed by
//! `loopr::daemon::startup::reconcile`. This crate does **not** depend on
//! `store`, does not own a registry file, and does not perform the crash-
//! recovery join itself — that's the `loopr` binary's job. See
//! `docs/design/2026-04-21-worktree-lifecycle.md`.

use std::path::Path;

use tracing::instrument;

use domain::WorkId;

mod config;
mod error;
mod excludes;
mod handle;
mod info;
mod ops;
mod parse;

pub use config::{AttemptCleanupPolicy, WorktreeConfig};
pub use error::WorktreeError;
pub use excludes::ensure_loopr_excludes;
pub use handle::Worktree;
pub use info::WorktreeInfo;

/// List loopr-managed worktrees visible in `repo_path`'s git registry,
/// filtered to paths under `worktree_root`. Entries for user-created
/// worktrees and the main checkout are dropped. Detached-HEAD entries are
/// also dropped (ours always carry a `loopr/wk-*` branch).
#[instrument(
    name = "worktree.list",
    level = "debug",
    skip_all,
    fields(repo_path = %repo_path.display(), worktree_root = %worktree_root.display(), count = tracing::field::Empty),
    err,
)]
pub fn list(repo_path: &Path, worktree_root: &Path) -> Result<Vec<WorktreeInfo>, WorktreeError> {
    let raw = ops::list_porcelain(repo_path)?;
    let result = parse::porcelain(&raw, worktree_root);
    tracing::Span::current().record("count", result.len());
    Ok(result)
}

/// Parse `loopr/wk-<work-id>-<seq>` → `(WorkId, seq)`. Returns `None` on
/// any shape deviation (missing prefix, missing seq, non-numeric seq, zero
/// seq). Reconcile uses this to identify which `Work` a surviving worktree
/// belongs to.
pub fn parse_branch(branch: &str) -> Option<(WorkId, u32)> {
    parse::branch(branch)
}

/// Remove a worktree at `worktree_path` without a handle. Used by reconcile,
/// which has a `WorktreeInfo` from `list()` but no `Worktree` handle. Keeps
/// the branch alive; call [`delete_branch`] explicitly if the branch should
/// also go.
#[instrument(
    name = "worktree.cleanup_at",
    level = "info",
    skip_all,
    fields(repo_path = %repo_path.display(), worktree_path = %worktree_path.display()),
    err,
)]
pub fn cleanup_at(repo_path: &Path, worktree_path: &Path) -> Result<(), WorktreeError> {
    // Finding 12: refuse to `git worktree remove --force` a path that is not
    // under a `.loopr/worktrees/` root. `cleanup_at` takes no handle, so this
    // is the only guard between a buggy caller and force-removing a user's own
    // worktree (or any registered worktree).
    if !under_worktrees_root(worktree_path) {
        return Err(WorktreeError::NotFound(worktree_path.to_path_buf()));
    }
    ops::remove_worktree(repo_path, worktree_path)
}

/// True if `path` contains a `.loopr` component immediately followed by a
/// `worktrees` component - i.e. it sits under a `.loopr/worktrees/` root.
fn under_worktrees_root(path: &Path) -> bool {
    let comps: Vec<&str> = path.components().filter_map(|c| c.as_os_str().to_str()).collect();
    comps.windows(2).any(|w| w == [".loopr", "worktrees"])
}

/// Delete a `loopr/wk-*` branch. Called by the integrator after a Tick
/// publishes (the Bundle's commits have landed on the integration branch),
/// by `loopr`'s live post-transition reap the instant a Work lands on a
/// terminal status (Phase 19), and by reconcile for any terminal Work
/// (`Done`, `Superseded`, `Abandoned`) as belt-and-suspenders. Idempotent
/// on missing.
#[instrument(
    name = "worktree.delete_branch",
    level = "info",
    skip_all,
    fields(repo_path = %repo_path.display(), branch),
    err,
)]
pub fn delete_branch(repo_path: &Path, branch: &str) -> Result<(), WorktreeError> {
    // Finding 12: only ever delete loopr-managed branches. A buggy caller
    // passing `main` (or any non-`loopr/` ref) is refused here rather than
    // force-deleting a real branch via `git branch -D`.
    if !branch.starts_with("loopr/") {
        return Err(WorktreeError::InvalidBranchName(branch.to_string()));
    }
    ops::delete_branch(repo_path, branch)
}

/// Resolve a ref (`HEAD`, a branch, a tag, a short SHA) to the full 40-char
/// SHA **in the repo's context**, not inside any worktree. This matters:
/// `HEAD` resolved inside a worktree returns that worktree's own branch
/// tip, which is exactly wrong when the Reactor wants the current
/// integration-branch tip to use as `sha` for the next attempt (D10;
/// v4 NO-OP-LOOP bug, commit `120c29b`).
#[instrument(
    name = "worktree.resolve_sha",
    level = "debug",
    skip_all,
    fields(repo_path = %repo_path.display(), base_ref),
    err,
)]
pub fn resolve_sha(repo_path: &Path, base_ref: &str) -> Result<String, WorktreeError> {
    ops::resolve_sha(repo_path, base_ref)
}

#[cfg(test)]
mod tests;
