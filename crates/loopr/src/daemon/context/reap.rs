//! Phase 19 (`docs/design/2026-07-11-verified-swarm.md`): live worktree +
//! branch reaping for a `Work` the instant it reaches a terminal status.
//!
//! Phase 10 extends the implementer worktree's lifetime to outlive review
//! (kept warm so the Reviewer's executed checks reuse incremental build
//! caches), deferring cleanup "until the Bundle reaches a terminal state."
//! Before this phase, nothing actually acted on that deferral at runtime:
//! the only code that ever swept a terminal Work's worktree was
//! `daemon::startup::sweep_worktrees`, which runs once, at daemon boot. A
//! Work that went Done (or was Director-abandoned) mid-run kept its warm
//! worktree on disk for the rest of the process's life. This module closes
//! that gap: `reap_terminal_work_worktree` is called immediately after the
//! transition that lands a Work on `is_terminal()`, from the same call
//! sites that used to leave the warm worktree in place.
//!
//! Extracted into its own file (alongside `spawner.rs` / `integration.rs`)
//! rather than added to `context.rs`, which is already at this crate's
//! per-file line ceiling.

use std::path::Path;

use domain::{WorkId, WorkStatus};
use tracing::{debug, instrument, warn};

/// Best-effort reap of every on-disk `loopr/wk-<work_id>-*` worktree +
/// branch belonging to `work_id`, called right after a `Work` lands on a
/// terminal status (`Done`, `Superseded`, `Abandoned`).
///
/// Keyed on the domain FSM's own `WorkStatus::is_terminal()` rather than a
/// hand-rolled status list, so this can never drift from the transition
/// table that governs which statuses are actually terminal. This is also
/// a hard invariant, not just caller discipline: a non-terminal `status`
/// (including `InReview` and `Blocked` -- Phase 10's retained,
/// awaiting-retry states) makes this a guaranteed no-op, so a Work still
/// in flight can never have its warm worktree pulled out from under it
/// even if a future call site gets the guard wrong.
///
/// Every failure here is a `warn!`, never a propagated error: the Work's
/// terminal transition has already persisted by the time this runs, and a
/// failed reap must not re-open or fail that already-terminal Work. The
/// startup reconcile sweep (`daemon::startup::sweep_worktrees`) is the
/// backstop for anything a live reap misses (a mid-cleanup crash, a
/// transient git failure, ...).
#[instrument(
    name = "daemon.reap_terminal_work_worktree",
    level = "debug",
    skip_all,
    fields(work_id = %work_id, status = ?status, reaped = tracing::field::Empty),
)]
pub(crate) async fn reap_terminal_work_worktree(target: &Path, work_id: &WorkId, status: WorkStatus) {
    if !status.is_terminal() {
        debug!("reap_terminal_work_worktree: status is not terminal; refusing to reap");
        return;
    }

    let worktree_root = target.join(".loopr").join("worktrees");
    if !worktree_root.exists() {
        return;
    }

    let infos = match worktree::list(target, &worktree_root) {
        Ok(infos) => infos,
        Err(e) => {
            warn!(error = %e, "reap_terminal_work_worktree: worktree::list failed; reconcile will sweep");
            return;
        }
    };

    let mut reaped = 0usize;
    for info in infos {
        let Some((wid, seq)) = worktree::parse_branch(&info.branch) else {
            continue;
        };
        if &wid != work_id {
            continue;
        }

        let repo = target.to_path_buf();
        let path = info.path.clone();
        match tokio::task::spawn_blocking(move || worktree::cleanup_at(&repo, &path)).await {
            Ok(Ok(())) => debug!(seq, path = %info.path.display(), "reap: removed terminal worktree"),
            Ok(Err(e)) => {
                warn!(seq, error = %e, "reap: cleanup_at failed (best-effort; reconcile will sweep)");
            }
            Err(join) => warn!(seq, error = %join, "reap: cleanup_at task panicked"),
        }

        let repo = target.to_path_buf();
        let branch = info.branch.clone();
        match tokio::task::spawn_blocking(move || worktree::delete_branch(&repo, &branch)).await {
            Ok(Ok(())) => debug!(seq, branch = %info.branch, "reap: deleted terminal branch"),
            Ok(Err(e)) => warn!(seq, error = %e, "reap: delete_branch failed (best-effort)"),
            Err(join) => warn!(seq, error = %join, "reap: delete_branch task panicked"),
        }
        reaped += 1;
    }
    tracing::Span::current().record("reaped", reaped);
}

#[cfg(test)]
mod tests;
