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

use std::path::{Path, PathBuf};

use domain::WorkId;

use crate::error::WorktreeError;
use crate::ops::{self, CreateOutcome};

/// Upper bound on the internal seq-retry loop. In practice a Work tops out
/// well under 10 attempts; 1000 is defensive against a stuck loop consuming
/// the full machine-side retry budget.
const MAX_SEQ: u32 = 1000;

pub struct Worktree {
    path: PathBuf,
    branch: String,
    work_id: WorkId,
    seq: u32,
    repo_path: PathBuf,
    base_sha: String,
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

    /// The commit SHA the worktree was branched from. Preserved for
    /// downstream diff operations (e.g., Implementer's
    /// `loc_changed = git diff --numstat <base_sha>..HEAD`).
    pub fn base_sha(&self) -> &str {
        &self.base_sha
    }

    /// Explicit cleanup. Removes the worktree (`git worktree remove --force`)
    /// and **keeps the branch** (integrator merges it after a Tick publishes).
    /// After this returns, the handle is marked consumed and `Drop` is a no-op.
    pub fn cleanup(mut self) -> Result<(), WorktreeError> {
        ops::remove_worktree(&self.repo_path, &self.path)?;
        self.consumed = true;
        Ok(())
    }

    /// Provision a fresh worktree + branch for `work_id`. The seq suffix is
    /// allocated internally by looping from 1 and retrying on git's
    /// "already exists" class of errors (locale-stable via `LC_ALL=C` in
    /// `ops::git_cmd`).
    ///
    /// Callers pass an already-resolved `base_sha` (not a ref) — the
    /// coordinator in `loopr` resolves it in repo context, NEVER inside a
    /// worktree, because `HEAD` inside a worktree resolves to the worktree's
    /// own branch tip rather than the intended base (D10; v4
    /// commit `120c29b`).
    ///
    /// `git worktree prune` runs ONCE at entry (D9). It is NOT called inside
    /// the retry loop: pruning mid-loop would create new race conditions.
    pub fn create(
        repo_path: &Path,
        worktree_root: &Path,
        work_id: WorkId,
        base_sha: &str,
    ) -> Result<Self, WorktreeError> {
        std::fs::create_dir_all(worktree_root)?;

        // D9: clear crashed-session registrations left behind under
        // $GIT_DIR/worktrees/. Non-fatal; logs-only on failure.
        ops::prune(repo_path)?;

        for seq in 1..=MAX_SEQ {
            match ops::try_create_at_seq(repo_path, worktree_root, &work_id, seq, base_sha)? {
                CreateOutcome::Created { path, branch } => {
                    verify_branch(&path, &branch)?;
                    return Ok(Self {
                        path,
                        branch,
                        work_id,
                        seq,
                        repo_path: repo_path.to_path_buf(),
                        base_sha: base_sha.to_string(),
                        consumed: false,
                    });
                }
                CreateOutcome::SeqTaken => continue,
            }
        }

        Err(WorktreeError::SeqAllocExhausted {
            attempts: MAX_SEQ,
            dir: worktree_root.to_path_buf(),
        })
    }

    /// Test-only constructor. Lets tests fabricate a handle with arbitrary
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
            base_sha: String::new(),
            consumed,
        }
    }
}

/// Post-create defensive check: inside the freshly-created worktree, the
/// current branch must match what we asked for. Ported from v4
/// manager.rs:122-132 — a failed branch creation could still produce a
/// worktree on the wrong branch if git's internal state is corrupted.
fn verify_branch(path: &Path, expected: &str) -> Result<(), WorktreeError> {
    let actual = ops::show_current_branch(path)?;
    if actual != expected {
        return Err(WorktreeError::GitCommand(format!(
            "worktree branch mismatch: expected {expected:?}, found {actual:?} at {}",
            path.display()
        )));
    }
    Ok(())
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
