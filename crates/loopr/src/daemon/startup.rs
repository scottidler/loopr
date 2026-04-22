//! Daemon startup sweep.
//!
//! `reconcile` runs ONCE at daemon startup before the IPC listener accepts,
//! joining `worktree::list(...)` output with TaskStore's `Work` records. Per
//! `docs/design/2026-04-21-worktree-lifecycle.md` D6, reconcile lives here
//! (in the binary crate) rather than inside the `worktree` crate because it
//! needs `Store` + `Work`-FSM awareness that `worktree` must not have.
//!
//! # Phase 4 scope (hygiene sweep)
//!
//! Stage 7's full reconcile ("mark crash-interrupted on non-terminal Works
//! whose worktrees survived a crash") depends on `Work.failure_reason` and a
//! `mark_crash_interrupted` mutator that do not yet exist in `domain`/`store`.
//! Until those land, this pass is the narrow subset that IS buildable today:
//!
//! 1. Enumerate worktrees under `<target>/.loopr/worktrees/`.
//! 2. Parse each branch name through `worktree::parse_branch`; skip
//!    non-loopr branches (humans may have created their own worktrees that
//!    happen to live under this root).
//! 3. Look up the `Work` in TaskStore. Missing record → log orphan.
//! 4. If `Work.status.is_terminal()` → `cleanup_at` the worktree and, if
//!    status is `Done`, `delete_branch` as belt-and-suspenders.
//! 5. Otherwise: log "surviving attempt" and leave alone — the next coordinator
//!    session (Stage 7) will deal with it.
//!
//! The `(non-terminal, crash-interrupted)` state mutation is explicitly a
//! Stage 7 follow-up; this file's doc calls it out so a future reader knows
//! where to wire it in.
//!
//! Single-threaded boot sequence: reconcile is called synchronously before
//! `bind_listener`, so there is no race with coordinators spawning new
//! attempts concurrently.

use std::path::Path;

use domain::WorkStatus;
use store::Store;

use crate::error::LooprError;

/// Summary of one reconcile pass, returned so the daemon can log it and
/// tests can assert on counts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Worktrees whose associated Work is terminal → cleaned + (maybe) branch deleted.
    pub cleaned: usize,
    /// Worktree entries we parsed but whose Work record is not in TaskStore.
    pub orphans_logged: usize,
    /// Non-terminal survivors left alone for the next coordinator session to handle.
    pub carried_forward: usize,
    /// Entries whose branch name did not match the `loopr/wk-*` shape.
    pub foreign_skipped: usize,
}

pub async fn reconcile(target: &Path, store: &Store) -> Result<ReconcileReport, LooprError> {
    let worktree_root = target.join(".loopr").join("worktrees");

    // Empty target (no worktrees yet) is the steady state for a fresh daemon
    // boot: short-circuit without touching git.
    if !worktree_root.exists() {
        tracing::debug!(
            worktree_root = %worktree_root.display(),
            "reconcile: worktree_root absent; nothing to sweep"
        );
        return Ok(ReconcileReport::default());
    }

    let infos = worktree::list(target, &worktree_root)
        .map_err(|e| LooprError::DaemonStartup(format!("worktree::list: {e}")))?;

    let mut report = ReconcileReport::default();
    for info in infos {
        let Some((work_id, seq)) = worktree::parse_branch(&info.branch) else {
            tracing::warn!(
                branch = %info.branch,
                path = %info.path.display(),
                "reconcile: foreign branch under worktree_root; skipping"
            );
            report.foreign_skipped += 1;
            continue;
        };

        match store.works().get(&work_id).await {
            Ok(work) => {
                if work.status.is_terminal() {
                    worktree::cleanup_at(target, &info.path)
                        .map_err(|e| LooprError::DaemonStartup(format!("cleanup_at: {e}")))?;
                    if matches!(work.status, WorkStatus::Done) {
                        worktree::delete_branch(target, &info.branch)
                            .map_err(|e| LooprError::DaemonStartup(format!("delete_branch: {e}")))?;
                    }
                    tracing::info!(
                        work_id = %work_id,
                        seq,
                        status = %work.status,
                        path = %info.path.display(),
                        "reconcile: cleaned terminal worktree"
                    );
                    report.cleaned += 1;
                } else {
                    tracing::info!(
                        work_id = %work_id,
                        seq,
                        status = %work.status,
                        path = %info.path.display(),
                        "reconcile: carrying forward non-terminal worktree (Stage 7 will mark crash-interrupted when that field exists)"
                    );
                    report.carried_forward += 1;
                }
            }
            Err(store::StoreError::RecordNotFound { .. }) => {
                tracing::warn!(
                    work_id = %work_id,
                    seq,
                    path = %info.path.display(),
                    "reconcile: worktree references a Work not in TaskStore; leaving for human review"
                );
                report.orphans_logged += 1;
            }
            Err(e) => {
                return Err(LooprError::DaemonStartup(format!("store.works().get({work_id}): {e}")));
            }
        }
    }

    tracing::info!(
        cleaned = report.cleaned,
        orphans = report.orphans_logged,
        carried_forward = report.carried_forward,
        foreign = report.foreign_skipped,
        "reconcile: pass complete"
    );
    Ok(report)
}

#[cfg(test)]
mod tests;
