//! `WorktreeInfo`: a single entry parsed from `git worktree list --porcelain`.
//!
//! v4-shape; consumed by `reconcile` in the `loopr` binary through
//! `worktree::list`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// Branch name without the `refs/heads/` prefix. `detached` worktrees
    /// are excluded at the parser level (they can't be ours — ours always
    /// carry a `loopr/wk-*` branch).
    pub branch: String,
    /// 40-char hex SHA of the worktree's HEAD commit.
    pub head: String,
}
