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
use std::sync::Arc;

use domain::{BundleStatus, Role, WorkStatus};
use llm::LlmClient;
use store::Store;

use crate::daemon::context::{DaemonContext, transition_and_persist_bundle};
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
    /// Stage 8 wiring capstone: Bundles at `Proposed` / `Triaged` re-enqueued
    /// for the Reviewer stage.
    pub reviewers_requeued: usize,
    /// Stage 8 wiring capstone: Bundles at `Reviewed` / `Accepted` /
    /// `Integrating` re-enqueued for the Integrator stage.
    pub integrators_requeued: usize,
    /// Stage 8 wiring capstone: Bundles already terminal; noop.
    pub bundles_terminal: usize,
}

#[tracing::instrument(name = "daemon.reconcile", level = "info", skip_all, fields(target = %ctx.target.display()), err)]
pub async fn reconcile<L>(ctx: &Arc<DaemonContext<L>>) -> Result<ReconcileReport, LooprError>
where
    L: LlmClient + Send + Sync + 'static,
{
    let mut report = sweep_worktrees(&ctx.target, &ctx.store).await?;

    // Stage 8 wiring capstone: Bundle-FSM sweep. Re-enqueue Bundles
    // stranded at intermediate statuses for the correct next stage.
    // Runs AFTER worktree hygiene and BEFORE `accept_loop` binds, so no
    // handler can race with the spawned tasks.
    sweep_bundles(ctx, &mut report).await?;

    tracing::info!(
        cleaned = report.cleaned,
        orphans = report.orphans_logged,
        carried_forward = report.carried_forward,
        foreign = report.foreign_skipped,
        reviewers_requeued = report.reviewers_requeued,
        integrators_requeued = report.integrators_requeued,
        bundles_terminal = report.bundles_terminal,
        "reconcile: pass complete"
    );
    Ok(report)
}

/// One-shot detector for a legacy pre-XDG `<target>/.loopr/runs/` dir.
///
/// Under the v5 Working Rule "no coexistence migrations", the daemon
/// does not auto-delete legacy state when the schema changes. Instead
/// it emits a single `warn!` at boot so the operator knows to clean it
/// up with `rkvr rmrf`, and then writes exclusively to XDG. A target
/// without `.loopr/runs/` returns `false` silently (fresh install or
/// already-cleaned).
///
/// Returns whether the legacy dir was present so callers (and tests)
/// can assert on the detection outcome. The warning itself is a side
/// effect and only observable by a connected tracing subscriber.
pub fn check_legacy_runs_dir(target: &Path) -> bool {
    let legacy = target.join(".loopr").join("runs");
    if legacy.is_dir() {
        tracing::warn!(
            path = %legacy.display(),
            "legacy runs dir present; no migration performed; rkvr rmrf to clean"
        );
        true
    } else {
        false
    }
}

/// Stage 7 worktree hygiene pass. Extracted from `reconcile` in Stage 8
/// so existing unit tests can exercise it without constructing a full
/// `DaemonContext`. Pure worktree/TaskStore logic; no task-spawn side
/// effects. `reconcile` calls this then `sweep_bundles`.
#[tracing::instrument(name = "daemon.sweep_worktrees", level = "debug", skip_all, fields(target = %target.display()), err)]
pub async fn sweep_worktrees(target: &Path, store: &Store) -> Result<ReconcileReport, LooprError> {
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

    Ok(report)
}

/// Enumerate persisted Bundles and re-enqueue each intermediate-state
/// Bundle into the correct daemon JoinSet. Terminal statuses noop.
///
/// A re-entry from `Reviewed` or `Accepted` requires a Coordinator
/// transition before the Integrator can consume the Bundle; we run it
/// in-place here (not inside `spawn_integrator_for_bundle`) because the
/// Integrator's pre-flight rejects anything that is not already at
/// `Accepted` or `Integrating`.
async fn sweep_bundles<L>(ctx: &Arc<DaemonContext<L>>, report: &mut ReconcileReport) -> Result<(), LooprError>
where
    L: LlmClient + Send + Sync + 'static,
{
    let bundles = match ctx.store.bundles().list().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "sweep_bundles: bundles().list() failed; skipping sweep");
            return Ok(());
        }
    };

    for bundle in bundles {
        match bundle.status {
            BundleStatus::Proposed | BundleStatus::Triaged => {
                let reviewer_ctx = Arc::clone(ctx);
                let mut rts = ctx.reviewer_tasks.lock().await;
                rts.spawn(reviewer_ctx.spawn_reviewer_for_bundle(bundle.clone()));
                tracing::info!(
                    bundle_id = %bundle.id,
                    status = ?bundle.status,
                    "sweep_bundles: requeued reviewer"
                );
                report.reviewers_requeued += 1;
            }
            BundleStatus::Reviewed => {
                // Coordinator transitions Reviewed -> Accepted in place so
                // the Integrator's pre-flight accepts the Bundle.
                let mut b = bundle.clone();
                if let Err(e) =
                    transition_and_persist_bundle(&ctx.store, &mut b, BundleStatus::Accepted, Role::Coordinator).await
                {
                    tracing::warn!(
                        bundle_id = %bundle.id,
                        error = %e,
                        "sweep_bundles: Reviewed -> Accepted transition failed; skipping"
                    );
                    continue;
                }
                let integrator_ctx = Arc::clone(ctx);
                let mut its = ctx.integrator_tasks.lock().await;
                its.spawn(integrator_ctx.spawn_integrator_for_bundle(b));
                tracing::info!(
                    bundle_id = %bundle.id,
                    "sweep_bundles: Reviewed -> Accepted, requeued integrator"
                );
                report.integrators_requeued += 1;
            }
            BundleStatus::Accepted | BundleStatus::Integrating => {
                let integrator_ctx = Arc::clone(ctx);
                let mut its = ctx.integrator_tasks.lock().await;
                its.spawn(integrator_ctx.spawn_integrator_for_bundle(bundle.clone()));
                tracing::info!(
                    bundle_id = %bundle.id,
                    status = ?bundle.status,
                    "sweep_bundles: requeued integrator"
                );
                report.integrators_requeued += 1;
            }
            BundleStatus::Merged
            | BundleStatus::Rejected
            | BundleStatus::IntegrationFailed
            | BundleStatus::Superseded => {
                report.bundles_terminal += 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
