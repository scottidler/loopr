//! `Worktree`: RAII handle to a per-attempt git worktree.
//!
//! Construction provisions the worktree + branch; explicit `.cleanup()`
//! removes the worktree but keeps the branch (the integrator still needs it
//! to merge). `Drop` is a **crash safety net**, not the routine cleanup
//! mechanism — the coordinator in `loopr` drives cleanup via explicit
//! `.cleanup()` inside `tokio::task::spawn_blocking` so a ~30ms `git worktree
//! remove --force` never runs on a tokio worker.
//!
//! The `consumed` flag prevents double-cleanup when a handle that's been
//! explicitly `.cleanup()`'d then hits Drop at scope exit.
//!
//! Phase 1: construction stubbed to `unimplemented!`; Phase 3 fills it in.

use std::path::{Path, PathBuf};

use domain::WorkId;

use crate::error::WorktreeError;
use crate::ops;

pub struct Worktree {
    path: PathBuf,
    branch: String,
    work_id: WorkId,
    seq: u32,
    repo_path: PathBuf,
    consumed: bool,
}

impl Worktree {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub fn work_id(&self) -> &WorkId {
        &self.work_id
    }

    pub fn seq(&self) -> u32 {
        self.seq
    }

    /// Explicit cleanup. Removes the worktree (`git worktree remove --force`)
    /// and **keeps the branch** (integrator merges it after a Tick publishes).
    /// After this returns, the handle is marked consumed and `Drop` is a no-op.
    pub fn cleanup(mut self) -> Result<(), WorktreeError> {
        ops::remove_worktree(&self.repo_path, &self.path)?;
        self.consumed = true;
        Ok(())
    }

    /// Internal constructor used by `Worktree::create` (Phase 3). Public-in-
    /// crate so tests in sibling modules can fabricate handles with custom
    /// `consumed` state without running the real git invocation.
    #[cfg(test)]
    pub(crate) fn from_parts(
        path: PathBuf,
        branch: String,
        work_id: WorkId,
        seq: u32,
        repo_path: PathBuf,
        consumed: bool,
    ) -> Self {
        Self {
            path,
            branch,
            work_id,
            seq,
            repo_path,
            consumed,
        }
    }

    /// Placeholder for Phase 3. The real body loops `seq` from 1 and retries on
    /// git's "already exists" class of errors.
    pub fn create(
        repo_path: &Path,
        worktree_root: &Path,
        work_id: WorkId,
        base_sha: &str,
    ) -> Result<Self, WorktreeError> {
        let _ = (repo_path, worktree_root, work_id, base_sha);
        unimplemented!("Worktree::create body lands in Phase 3")
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        // Best-effort synchronous cleanup. Logs on failure; does not panic.
        // Coordinator is expected to use explicit `.cleanup()` inside
        // `spawn_blocking` for routine sweeps; this path is the crash /
        // panic-unwind safety net.
        if let Err(e) = ops::remove_worktree(&self.repo_path, &self.path) {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "Worktree Drop cleanup failed (non-fatal; reconcile will sweep on next startup)"
            );
        }
    }
}

#[cfg(test)]
mod tests;
