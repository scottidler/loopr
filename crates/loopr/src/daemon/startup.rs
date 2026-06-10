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
//! 5. Otherwise: log "surviving attempt" and leave alone — the next reactor
//!    session (Stage 7) will deal with it.
//!
//! The `(non-terminal, crash-interrupted)` state mutation is explicitly a
//! Stage 7 follow-up; this file's doc calls it out so a future reader knows
//! where to wire it in.
//!
//! Single-threaded boot sequence: reconcile is called synchronously before
//! `bind_listener`, so there is no race with reactors spawning new
//! attempts concurrently.

use std::path::Path;
use std::sync::Arc;

use agents::{DirectorDeps, DirectorError, run_director};
use domain::{BundleStatus, FailureReason, PlanStatus, WorkStatus};
use futures_util::FutureExt;
use llm::LlmClient;
use store::Store;

use crate::daemon::context::{DaemonContext, DaemonSpawner, block_dependent_siblings, promote_unblocked_siblings};
use crate::error::LooprError;

/// Summary of one reconcile pass, returned so the daemon can log it and
/// tests can assert on counts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Worktrees whose associated Work is terminal → cleaned + (maybe) branch deleted.
    pub cleaned: usize,
    /// Worktree entries we parsed but whose Work record is not in TaskStore.
    pub orphans_logged: usize,
    /// Non-terminal survivors left alone for the next reactor session to handle.
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
    /// Tier-1-cleanup: corrupt JSONL rows surfaced by `list_tolerant`
    /// during the sweep. Aggregated across the Work and Bundle passes.
    /// Drives the daemon-boot corruption gate (refuse-to-listen unless
    /// `--accept-corruption`).
    pub corruption_count: usize,
}

/// Emit one structured `error!` per corrupt JSONL row surfaced by a
/// `list_tolerant` call. Shared between the Work and Bundle sweeps so
/// the log shape is identical.
fn log_corruption(record_kind: &'static str, corruption: &[store::CorruptionEntry]) {
    for entry in corruption {
        tracing::error!(
            record_kind,
            file = %entry.file.display(),
            line = entry.line,
            error = ?entry.error,
            "corrupt record skipped during sweep"
        );
    }
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

    // Dep gate: crash-recovery promotion sweep. For each Active Plan,
    // promote any Pending Works whose deps are now all Done. This closes
    // the gap where a dep went Done before a crash and left its
    // dependents stranded in Pending forever.
    sweep_dep_promotions(ctx).await;

    // Director Phase 3 wiring: respawn a Director task for every
    // non-terminal Plan so a daemon restart resumes per-Plan supervision
    // without manual intervention.
    startup_reconcile_directors(ctx).await;

    tracing::info!(
        cleaned = report.cleaned,
        orphans = report.orphans_logged,
        carried_forward = report.carried_forward,
        foreign = report.foreign_skipped,
        reviewers_requeued = report.reviewers_requeued,
        integrators_requeued = report.integrators_requeued,
        bundles_terminal = report.bundles_terminal,
        corruption_count = report.corruption_count,
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
    let mut report = ReconcileReport::default();

    // Tolerant pre-pass over the Work JSONL: surface any malformed rows
    // as `corruption_count` BEFORE any SQLite-cache-backed lookup runs.
    // A JSONL-malformed Work is silently dropped by `sync()` and never
    // reaches SQLite, so the per-id `get(work_id)` path below would
    // return `Ok(None)` and the gate would otherwise stay blind. The
    // returned records are intentionally ignored: per-worktree matching
    // still uses parsed branch names below, not a flat scan.
    match store.works().list_tolerant(&[]).await {
        Ok(result) => {
            log_corruption("work", &result.corruption);
            report.corruption_count += result.corruption.len();
        }
        Err(e) => {
            tracing::warn!(error = %e, "sweep_worktrees: works().list_tolerant failed; corruption gate may be undercounted");
        }
    }

    let worktree_root = target.join(".loopr").join("worktrees");

    // Empty target (no worktrees yet) is the steady state for a fresh daemon
    // boot: short-circuit without touching git.
    if !worktree_root.exists() {
        tracing::debug!(
            worktree_root = %worktree_root.display(),
            "reconcile: worktree_root absent; nothing to sweep"
        );
        return Ok(report);
    }

    let infos = worktree::list(target, &worktree_root)
        .map_err(|e| LooprError::DaemonStartup(format!("worktree::list: {e}")))?;
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
            Ok(mut work) => {
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
                    // A non-terminal Work with a worktree on disk at boot
                    // means a prior daemon crashed mid-flight. Record the
                    // typed `CrashInterrupted` reason (the placeholder this
                    // log line described for stages) so the carry-forward is
                    // machine-visible, not only a log entry. Idempotent: skip
                    // the write if already stamped so repeated boots don't
                    // churn `updated_at`. Best-effort — a failed write logs
                    // and still carries the worktree forward.
                    if work.failure_reason != Some(FailureReason::CrashInterrupted) {
                        let expected = work.updated_at;
                        work.failure_reason = Some(FailureReason::CrashInterrupted);
                        if let Err(e) = store.works().update(work.clone(), expected).await {
                            tracing::warn!(
                                work_id = %work_id,
                                error = %e,
                                "reconcile: failed to stamp CrashInterrupted; carrying forward anyway"
                            );
                        }
                    }
                    tracing::info!(
                        work_id = %work_id,
                        seq,
                        status = %work.status,
                        path = %info.path.display(),
                        "reconcile: carrying forward non-terminal worktree (failure_reason=crash-interrupted)"
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
/// A re-entry from `Reviewed` or `Accepted` requires a Reactor
/// transition before the Integrator can consume the Bundle; we run it
/// in-place here (not inside `spawn_integrator_for_bundle`) because the
/// Integrator's pre-flight rejects anything that is not already at
/// `Accepted` or `Integrating`.
async fn sweep_bundles<L>(ctx: &Arc<DaemonContext<L>>, report: &mut ReconcileReport) -> Result<(), LooprError>
where
    L: LlmClient + Send + Sync + 'static,
{
    let bundles = match ctx.store.bundles().list_tolerant(&[]).await {
        Ok(result) => {
            log_corruption("bundle", &result.corruption);
            report.corruption_count += result.corruption.len();
            result.records
        }
        Err(e) => {
            tracing::warn!(error = %e, "sweep_bundles: bundles().list_tolerant() failed; skipping sweep");
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
                // Director Phase 3: Reviewed bundles wait for the
                // per-Plan Director to emit `accept_bundle`. The startup
                // hook below (`startup_reconcile_directors`) respawns
                // a Director for every Active Plan, so a Bundle stranded
                // at Reviewed across a daemon restart is picked up on the
                // Director's first poll. The Stage 8 inline auto-accept
                // is intentionally removed here.
                tracing::info!(
                    bundle_id = %bundle.id,
                    "sweep_bundles: Reviewed bundle awaiting Director acceptance"
                );
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

/// Crash-recovery dep promotion sweep. For each Active Plan, calls
/// `promote_unblocked_siblings` so any Works whose deps went Done
/// before the crash but were never promoted get their Implementers
/// spawned now. Best-effort: failures are logged and skipped.
async fn sweep_dep_promotions<L>(ctx: &Arc<DaemonContext<L>>)
where
    L: LlmClient + Send + Sync + 'static,
{
    let plans = match ctx.store.plans().list().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "sweep_dep_promotions: plans().list() failed; skipping");
            return;
        }
    };
    let active_plans: Vec<_> = plans.into_iter().filter(|p| p.status == PlanStatus::Active).collect();
    tracing::debug!(
        plan_count = active_plans.len(),
        "sweep_dep_promotions: scanning Active Plans"
    );
    for plan in active_plans {
        let plan_id = plan.id.clone();
        // Promotion: spawn implementers for newly unblocked Works.
        promote_unblocked_siblings(Arc::clone(ctx), plan_id.clone()).await;
        // Blocking: mark Pending Works whose deps are irrecoverably terminal.
        // Fetch siblings once for the blocking sweep.
        if let Ok(siblings) = ctx.store.works().list_by_parent_id(&plan_id).await {
            for work in &siblings {
                if let Some(terminal_dep_id) = work.any_dep_irrecoverable(&siblings) {
                    let terminal_dep = siblings.iter().find(|s| &s.id == terminal_dep_id);
                    if let Some(dep) = terminal_dep {
                        block_dependent_siblings(Arc::clone(ctx), plan_id.clone(), dep.id.clone(), dep.status).await;
                    }
                }
            }
        }
    }
}

/// Director Phase 3 startup hook. Lists every Active Plan and spawns a
/// `run_director` task into `ctx.director_tasks`. A daemon restart with an
/// in-flight Plan resumes Director supervision without manual
/// intervention. Best-effort: store errors are logged and skipped so a
/// single bad Plan does not block boot.
async fn startup_reconcile_directors<L>(ctx: &Arc<DaemonContext<L>>)
where
    L: LlmClient + Send + Sync + 'static,
{
    let plans = match ctx.store.plans().list().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "startup_reconcile_directors: plans().list() failed; skipping");
            return;
        }
    };
    let active: Vec<_> = plans.into_iter().filter(|p| p.status == PlanStatus::Active).collect();
    tracing::info!(
        plan_count = active.len(),
        "startup_reconcile_directors: scanning Active Plans"
    );
    for plan in active {
        let plan_id = plan.id.clone();
        // Bullet 13: an Active Plan with zero Works was stalled during or
        // before decomposition — a shutdown/drain that skipped
        // `decompose_and_dispatch`, or a crash mid-decompose. Re-enter
        // `decompose_and_dispatch` (which re-decomposes, persists Works,
        // spawns Implementers, AND spawns the Director) instead of
        // spawning a Director to supervise nothing. A store error here
        // falls through to the normal Director spawn (safe default: don't
        // re-decompose on uncertainty).
        let zero_works = matches!(ctx.store.works().list_by_parent_id(&plan_id).await, Ok(w) if w.is_empty());
        if zero_works {
            tracing::info!(plan_id = %plan_id, "startup_reconcile_directors: Active Plan with zero Works; re-decomposing");
            let task_ctx = Arc::clone(ctx);
            ctx.plan_create_tasks.lock().await.spawn(async move {
                if task_ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                crate::transport::handler::decompose_and_dispatch(&task_ctx, plan, 0).await;
            });
            continue;
        }
        // Budget soft pause (vision Budgets): a daemon that booted already
        // over its per-run cap must not reconcile a fresh Director.
        if ctx.budget_blocks_spawn("director", plan_id.as_ref()) {
            continue;
        }
        let operator_notify = Arc::new(tokio::sync::Notify::new());
        ctx.operator_notifies
            .write()
            .await
            .insert(plan_id.clone(), Arc::clone(&operator_notify));
        let deps = DirectorDeps {
            llm: Arc::clone(&ctx.llm),
            store: Arc::clone(&ctx.store),
            context: Arc::clone(&ctx.context_builder),
            spawner: DaemonSpawner(Arc::clone(ctx)),
            config: ctx.director_config.clone(),
            shutdown: Arc::clone(&ctx.shutdown_notify),
            operator_notify,
            director_statuses: Arc::clone(&ctx.director_statuses),
        };
        let mut directors = ctx.director_tasks.lock().await;
        let plan_id_for_log = plan_id.clone();
        let operator_notifies = Arc::clone(&ctx.operator_notifies);
        let director_statuses = Arc::clone(&ctx.director_statuses);
        let plan_id_for_cleanup = plan_id.clone();
        // Compare-before-remove token (see spawn_director_for_plan): a
        // respawned Director may replace this Notify before our cleanup.
        let notify_for_cleanup = Arc::clone(&deps.operator_notify);
        directors.spawn(async move {
            // Panic posture: `catch_unwind` so a panic inside
            // `run_director` is logged and the per-Plan Notify +
            // status-snapshot cleanup below still runs.
            let call_ctx = llm::CallContext {
                plan_id: Some(plan_id.to_string()),
                work_id: None,
                role: Some("director".to_string()),
            };
            let result =
                std::panic::AssertUnwindSafe(llm::CallContext::scope(call_ctx, run_director(&plan_id, &deps)))
                    .catch_unwind()
                    .await;
            match result {
                Ok(Ok(())) => tracing::info!(plan_id = %plan_id_for_log, "director task exited Ok"),
                Ok(Err(DirectorError::NeedHelp(reason))) => tracing::warn!(
                    plan_id = %plan_id_for_log,
                    reason = %reason,
                    "director exited with NeedHelp"
                ),
                Ok(Err(e)) => tracing::error!(plan_id = %plan_id_for_log, error = %e, "director exited with error"),
                Err(panic) => {
                    let msg = crate::daemon::context::panic_message(&*panic);
                    tracing::error!(plan_id = %plan_id_for_log, panic = %msg, "director task panicked");
                }
            }
            // Phase 9: drop the per-Plan operator Notify on Director task
            // exit, but ONLY if the map still holds the Notify THIS task
            // inserted (compare-before-remove) — a Stalled -> Active
            // override may have respawned a Director with a fresh Notify.
            {
                let mut map = operator_notifies.write().await;
                if map
                    .get(&plan_id_for_cleanup)
                    .is_some_and(|n| Arc::ptr_eq(n, &notify_for_cleanup))
                {
                    map.remove(&plan_id_for_cleanup);
                }
            }
            // Director Phase 2 follow-ups (Item 3): drop the per-Plan
            // status snapshot on task exit so a subsequent
            // `director.status` IPC call returns the "not running"
            // wire form instead of stale data.
            if let Ok(mut m) = director_statuses.write() {
                m.remove(&plan_id_for_cleanup);
            }
        });
    }
}

#[cfg(test)]
mod tests;
