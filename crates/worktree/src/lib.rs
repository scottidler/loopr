//! Per-attempt git worktree lifecycle. Infrastructure-only.
//!
//! Exposes the `Worktree` RAII handle, the `AttemptCleanupPolicy` enum + its
//! `WorktreeConfig` wrapper, and a set of free functions consumed by
//! `loopr::daemon::startup::reconcile`. This crate does **not** depend on
//! `store`, does not own a registry file, and does not perform the crash-
//! recovery join itself — that's the `loopr` binary's job. See
//! `docs/design/2026-04-21-worktree-lifecycle.md`.

use std::path::Path;

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
pub fn list(repo_path: &Path, worktree_root: &Path) -> Result<Vec<WorktreeInfo>, WorktreeError> {
    let raw = ops::list_porcelain(repo_path)?;
    Ok(parse::porcelain(&raw, worktree_root))
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
pub fn cleanup_at(repo_path: &Path, worktree_path: &Path) -> Result<(), WorktreeError> {
    ops::remove_worktree(repo_path, worktree_path)
}

/// Delete a `loopr/wk-*` branch. Called by the integrator after a Tick
/// publishes (the Bundle's commits have landed on the integration branch),
/// and by reconcile for terminal `Done` Works as belt-and-suspenders.
/// Idempotent on missing.
pub fn delete_branch(repo_path: &Path, branch: &str) -> Result<(), WorktreeError> {
    ops::delete_branch(repo_path, branch)
}

/// Resolve a ref (`HEAD`, a branch, a tag, a short SHA) to the full 40-char
/// SHA **in the repo's context**, not inside any worktree. This matters:
/// `HEAD` resolved inside a worktree returns that worktree's own branch
/// tip, which is exactly wrong when the coordinator wants the current
/// integration-branch tip to use as `sha` for the next attempt (D10;
/// v4 NO-OP-LOOP bug, commit `120c29b`).
pub fn resolve_sha(repo_path: &Path, base_ref: &str) -> Result<String, WorktreeError> {
    ops::resolve_sha(repo_path, base_ref)
}
