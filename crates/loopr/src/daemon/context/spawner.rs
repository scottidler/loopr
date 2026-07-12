//! `WorkSpawner` implementation for the daemon. Extracted from the parent
//! `context` module to keep `context.rs` under the per-file line limit.
//!
//! `DaemonSpawner` is a thin newtype around `Arc<DaemonContext<L>>` that
//! satisfies Rust's orphan rule for the `agents::WorkSpawner` trait. The
//! impl bridges the sync trait surface to the daemon's async task pools
//! (`work_spawner_tasks`, `reviewer_tasks`, `integrator_tasks`,
//! `implementer_tasks`) via a two-layer `tokio::spawn` shim.
//!
//! Phase 2 of `docs/design/2026-05-09-director-phase-2.md` adds the
//! stuck-state recovery surface: `spawn_reviewer`, `spawn_integrator`,
//! and three `list_running_*_ids` helpers backed by the parent module's
//! sidecar maps.

use std::sync::Arc;

use agents::WorkSpawner;
use domain::{BundleId, BundleStatus, Role, TargetKind, WorkId, WorkStatus, decide_accept};
use llm::LlmClient;
use tracing::{debug, info, warn};

use super::{DaemonContext, TransitionError, block_dependent_siblings, transition_and_persist_work};

// ---------------------------------------------------------------------------
// WorkSpawner: Director's fire-and-forget surface into the daemon.
// ---------------------------------------------------------------------------

/// Thin wrapper around `Arc<DaemonContext<L>>` so `WorkSpawner` (defined
/// in `agents`) can be implemented for a local type. Rust's orphan rule
/// forbids `impl WorkSpawner for Arc<DaemonContext<L>>` directly because
/// neither `WorkSpawner` nor `Arc` is local to this crate. The newtype
/// is local; `WorkSpawner` becomes a one-line forwarding impl.
pub struct DaemonSpawner<L: LlmClient + Send + Sync + 'static>(pub Arc<DaemonContext<L>>);

impl<L: LlmClient + Send + Sync + 'static> Clone for DaemonSpawner<L> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// `WorkSpawner` impl on `DaemonSpawner<L>`. Each method clones the inner
/// Arc and spawns a tokio task into the relevant `*_tasks` JoinSet so
/// the daemon's drain ordering still applies.
///
/// Stage 8 used to fire `Reviewed -> Accepted` + spawn Integrator inline
/// from `spawn_reviewer_for_bundle`; Phase 3 hands that decision to the
/// Director. `accept_bundle` is the resulting code path.
impl<L> WorkSpawner for DaemonSpawner<L>
where
    L: LlmClient + Send + Sync + 'static,
{
    fn accept_bundle(&self, bundle_id: BundleId) {
        let ctx_for_lock = Arc::clone(&self.0);
        let ctx = Arc::clone(&self.0);
        // Shim: bridge the sync `WorkSpawner` trait to the async
        // `work_spawner_tasks` lock. The shim itself runs as a detached
        // tokio task; on shutdown it observes `shutting_down` either via
        // the early-return below or via the no-op behavior of the inner
        // task body. The inner spawn lands in `work_spawner_tasks` so
        // the daemon's drain order can join it deterministically.
        tokio::spawn(async move {
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let mut tasks = ctx_for_lock.work_spawner_tasks.lock().await;
            // Re-check under the lock: the shutdown drain holds this same
            // lock while draining, and `shutting_down` is set before any
            // drain runs, so a shim that passed the pre-lock check could
            // otherwise insert into an already-drained JoinSet
            // (check-then-lock race). Observing it true here means skip.
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tasks.spawn(async move {
                if ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                    debug!(bundle_id = %bundle_id, "shutdown in progress; skipping accept_bundle");
                    return;
                }
                // Re-read Bundle: the Director's loop snapshot is stale by
                // the time the action lands here.
                let mut bundle = match ctx.store.bundles().get(&bundle_id).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(error = %e, bundle_id = %bundle_id, "accept_bundle: bundle lookup failed");
                        return;
                    }
                };
                // Idempotent: a Director restart after a clear-history can
                // re-emit `accept_bundle` for a Bundle already at Accepted.
                // No-op silently on already-Accepted; warn on anything else.
                match bundle.status {
                    BundleStatus::Accepted => {
                        debug!(bundle_id = %bundle_id, "accept_bundle: already Accepted; no-op");
                        return;
                    }
                    BundleStatus::Reviewed => {}
                    other => {
                        warn!(
                            bundle_id = %bundle_id,
                            status = ?other,
                            "accept_bundle: unexpected status; skipping"
                        );
                        return;
                    }
                }
                // Phase 11 deterministic accept gate (panel must-fix #1). The
                // prompt is not the gate; THIS is. `Reviewed -> Accepted` is
                // refused unless the persisted latest Review for the Bundle is
                // `Accept` with zero red referenced CheckRuns. Missing, stale
                // (round mismatch), or red evidence -> no accept; the Bundle
                // stays Reviewed and the Director state summary flags it as
                // evidence-broken (the Director re-reviews or escalates).
                let reviews = match ctx.store.reviews().list_by_bundle(&bundle_id).await {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(error = %e, bundle_id = %bundle_id, "accept_bundle: review evidence lookup failed; refusing accept (fail-closed)");
                        return;
                    }
                };
                let check_runs = match ctx.store.check_runs().list_by_bundle(&bundle_id).await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(error = %e, bundle_id = %bundle_id, "accept_bundle: check-run evidence lookup failed; refusing accept (fail-closed)");
                        return;
                    }
                };
                let decision = decide_accept(&reviews, &check_runs);
                if !decision.is_accept() {
                    warn!(
                        bundle_id = %bundle_id,
                        evidence = %decision.evidence_label(),
                        "accept_bundle: REFUSED by the deterministic accept gate; Bundle stays Reviewed (evidence-broken)"
                    );
                    return;
                }
                debug!(
                    bundle_id = %bundle_id,
                    evidence = %decision.evidence_label(),
                    "accept_bundle: accept gate passed; proceeding to Accepted"
                );
                let expected = bundle.updated_at;
                if let Err(e) = bundle.transition(BundleStatus::Accepted, Role::Director) {
                    warn!(error = %e, bundle_id = %bundle_id, "accept_bundle: FSM transition rejected");
                    return;
                }
                if let Err(e) = ctx
                    .store
                    .bundles()
                    .update(bundle.clone(), expected, Role::Director, TargetKind::Normal)
                    .await
                {
                    // Stale OCC errors are expected when the daemon's reconcile
                    // sweep races the Director; swallow and continue.
                    if let store::StoreError::Stale { .. } = e {
                        debug!(bundle_id = %bundle_id, "accept_bundle: OCC Stale; another writer beat us");
                        return;
                    }
                    warn!(error = %e, bundle_id = %bundle_id, "accept_bundle: OCC update failed");
                    return;
                }
                // Phase 4: `Reviewed -> Accepted` just persisted successfully.
                if let Ok(mut snap) = ctx.snapshot.lock() {
                    snap.bundles_accepted += 1;
                } else {
                    warn!("accept_bundle: snapshot Mutex poisoned; bundles_accepted dropped");
                }
                // Spawn Integrator into the existing pool so the drain order
                // still applies.
                let integrator_ctx = Arc::clone(&ctx);
                let mut its = ctx.integrator_tasks.lock().await;
                its.spawn(integrator_ctx.spawn_integrator_for_bundle(bundle));
            });
        });
    }

    fn override_work(&self, work_id: WorkId, target_status: WorkStatus, reason: String) {
        let ctx_for_lock = Arc::clone(&self.0);
        let ctx = Arc::clone(&self.0);
        // Shim: see `accept_bundle` above for the bridging rationale.
        tokio::spawn(async move {
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let mut tasks = ctx_for_lock.work_spawner_tasks.lock().await;
            // Re-check under the lock: the shutdown drain holds this same
            // lock while draining, and `shutting_down` is set before any
            // drain runs, so a shim that passed the pre-lock check could
            // otherwise insert into an already-drained JoinSet
            // (check-then-lock race). Observing it true here means skip.
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tasks.spawn(async move {
                if ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                    debug!(work_id = %work_id, "shutdown in progress; skipping override_work");
                    return;
                }
                let mut work = match ctx.store.works().get(&work_id).await {
                    Ok(w) => w,
                    Err(e) => {
                        warn!(error = %e, work_id = %work_id, "override_work: work lookup failed");
                        return;
                    }
                };
                // FSM is the source of truth for what's permitted; the impl
                // logs the reason so an operator scrolling logs sees why the
                // Director moved the Work.
                info!(
                    work_id = %work_id,
                    from = ?work.status,
                    to = ?target_status,
                    reason = %reason,
                    "override_work: applying Director-issued FSM override"
                );
                // Gate the spawn on PERSIST SUCCESS. The pre-fix code
                // spawned an Implementer off the locally-mutated
                // `work.status == Ready` even when the persist FAILED —
                // spawning for a Work whose persisted state belongs to the
                // racing winner (OCC Stale) or whose write erred. A Stale
                // is benign (another writer already advanced the Work), so
                // it logs at debug and simply does not spawn.
                let persisted = match transition_and_persist_work(
                    &*ctx.summary_fanout,
                    &mut work,
                    target_status,
                    Role::Director,
                    true, // override
                    &ctx.snapshot,
                )
                .await
                {
                    Ok(()) => true,
                    Err(TransitionError::Stale { .. }) => {
                        debug!(work_id = %work_id, "override_work: OCC Stale; another writer won, not spawning");
                        false
                    }
                    Err(e) => {
                        warn!(error = %e, work_id = %work_id, "override_work: persist failed; not spawning");
                        false
                    }
                };
                // If the override persisted AND pushed the Work to Ready,
                // kick the Implementer pipeline. The Director's primary
                // recovery path is `Blocked -> Ready` to retry a
                // previously-rejected Bundle; without this spawn, the Work
                // would sit Ready forever until a sibling completion
                // triggered `promote_unblocked_siblings`.
                if persisted
                    && work.status == WorkStatus::Ready
                    && !ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed)
                {
                    ctx.spawn_implementer_registered(work).await;
                } else if persisted && matches!(work.status, WorkStatus::Abandoned | WorkStatus::Superseded) {
                    // F7: a Director override that terminalizes the Work
                    // (Abandoned/Superseded) at runtime must block its
                    // transitive Pending dependents now — otherwise they
                    // strand until the next daemon restart's reconcile.
                    block_dependent_siblings(Arc::clone(&ctx), work.parent_id.clone(), work.id.clone(), work.status)
                        .await;
                }
            });
        });
    }

    fn assign_work(&self, work_id: WorkId) {
        let ctx_for_lock = Arc::clone(&self.0);
        let ctx = Arc::clone(&self.0);
        // Shim: see `accept_bundle` above for the bridging rationale.
        tokio::spawn(async move {
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let mut tasks = ctx_for_lock.work_spawner_tasks.lock().await;
            // Re-check under the lock: the shutdown drain holds this same
            // lock while draining, and `shutting_down` is set before any
            // drain runs, so a shim that passed the pre-lock check could
            // otherwise insert into an already-drained JoinSet
            // (check-then-lock race). Observing it true here means skip.
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tasks.spawn(async move {
                if ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                    debug!(work_id = %work_id, "shutdown in progress; skipping assign_work");
                    return;
                }
                let work = match ctx.store.works().get(&work_id).await {
                    Ok(w) => w,
                    Err(e) => {
                        warn!(error = %e, work_id = %work_id, "assign_work: work lookup failed");
                        return;
                    }
                };
                // Dep-gate: re-read siblings and confirm every dep is Done.
                // Director may emit `assign_work` for a Work whose deps are
                // not yet complete; the contract is to silently no-op rather
                // than spawn a doomed Implementer.
                let siblings = match ctx.store.works().list_by_parent_id(&work.parent_id).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, work_id = %work_id, "assign_work: sibling list failed");
                        return;
                    }
                };
                if !work.all_deps_done(&siblings) {
                    warn!(work_id = %work_id, "assign_work: deps not all Done; ignoring");
                    return;
                }
                // Only Pending or Ready Works are eligible; everything else
                // means the dep-gate reactive path or Director already moved
                // on. No-op without churning the FSM.
                if !matches!(work.status, WorkStatus::Pending | WorkStatus::Ready) {
                    debug!(work_id = %work_id, status = ?work.status, "assign_work: not eligible; skipping");
                    return;
                }
                ctx.spawn_implementer_registered(work).await;
            });
        });
    }

    // ---------- Phase 2 stuck-state recovery surface ----------

    /// Re-spawn a Reviewer task for a Bundle stuck at `Triaged`. The
    /// reconcile sweep fires this after detecting Triaged-no-live-Reviewer
    /// past the grace window. Re-running the Reviewer on Reviewed (or
    /// later) Bundles would redo work, so we filter explicitly.
    fn spawn_reviewer(&self, bundle_id: BundleId) {
        let ctx_for_lock = Arc::clone(&self.0);
        let ctx = Arc::clone(&self.0);
        // Shim: see `accept_bundle` above for the bridging rationale.
        tokio::spawn(async move {
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let mut tasks = ctx_for_lock.work_spawner_tasks.lock().await;
            // Re-check under the lock: the shutdown drain holds this same
            // lock while draining, and `shutting_down` is set before any
            // drain runs, so a shim that passed the pre-lock check could
            // otherwise insert into an already-drained JoinSet
            // (check-then-lock race). Observing it true here means skip.
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tasks.spawn(async move {
                if ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                    debug!(bundle_id = %bundle_id, "shutdown in progress; skipping spawn_reviewer");
                    return;
                }
                let bundle = match ctx.store.bundles().get(&bundle_id).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(error = %e, bundle_id = %bundle_id, "spawn_reviewer: bundle lookup failed");
                        return;
                    }
                };
                // Triaged-only: re-running on Reviewed/Accepted/Integrating/
                // Merged would be wasted work or a contract violation.
                // Proposed is also valid here because spawn_reviewer_for_bundle
                // handles the Proposed -> Triaged triage step itself.
                if !matches!(bundle.status, BundleStatus::Proposed | BundleStatus::Triaged) {
                    debug!(
                        bundle_id = %bundle_id,
                        status = ?bundle.status,
                        "spawn_reviewer: bundle not in re-spawn-eligible state; skipping"
                    );
                    return;
                }
                let reviewer_ctx = Arc::clone(&ctx);
                let mut rts = ctx.reviewer_tasks.lock().await;
                rts.spawn(reviewer_ctx.spawn_reviewer_for_bundle(bundle));
            });
        });
    }

    /// Re-spawn an Integrator task for a Bundle stuck at `Accepted` with
    /// no live Integrator. Distinct from `accept_bundle`'s already-Accepted
    /// no-op branch (which by design does NOT spawn Integrator) so the
    /// reconcile sweep has a deterministic recovery path for the
    /// Accepted-no-Integrator case.
    fn spawn_integrator(&self, bundle_id: BundleId) {
        let ctx_for_lock = Arc::clone(&self.0);
        let ctx = Arc::clone(&self.0);
        tokio::spawn(async move {
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let mut tasks = ctx_for_lock.work_spawner_tasks.lock().await;
            // Re-check under the lock: the shutdown drain holds this same
            // lock while draining, and `shutting_down` is set before any
            // drain runs, so a shim that passed the pre-lock check could
            // otherwise insert into an already-drained JoinSet
            // (check-then-lock race). Observing it true here means skip.
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tasks.spawn(async move {
                if ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                    debug!(bundle_id = %bundle_id, "shutdown in progress; skipping spawn_integrator");
                    return;
                }
                let bundle = match ctx.store.bundles().get(&bundle_id).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(error = %e, bundle_id = %bundle_id, "spawn_integrator: bundle lookup failed");
                        return;
                    }
                };
                // Accepted-only: spawning on Reviewed would skip the FSM
                // transition; spawning on Integrating means an Integrator
                // is already in flight.
                if bundle.status != BundleStatus::Accepted {
                    debug!(
                        bundle_id = %bundle_id,
                        status = ?bundle.status,
                        "spawn_integrator: bundle not in Accepted state; skipping"
                    );
                    return;
                }
                let integrator_ctx = Arc::clone(&ctx);
                let mut its = ctx.integrator_tasks.lock().await;
                its.spawn(integrator_ctx.spawn_integrator_for_bundle(bundle));
            });
        });
    }

    /// Snapshot of Work IDs currently running an Implementer task.
    /// Blocking `read()` on the std `RwLock`: the lock is held only for
    /// microsecond inserts/removes from spawn-wrapper RAII guards, so
    /// contention is negligible. `try_read` was rejected because a
    /// single failed read would return an empty Vec and the reconcile
    /// sweep would mass-respawn every past-grace InProgress Work in the
    /// Plan. Poison degrades to empty (same hazard as a read failure)
    /// but only on actual writer panic, not routine contention.
    fn list_running_work_ids(&self) -> Vec<WorkId> {
        self.0
            .implementer_work_ids
            .read()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Snapshot of Bundle IDs currently running a Reviewer task. See
    /// `list_running_work_ids` for lock-strategy rationale.
    fn list_running_reviewer_bundle_ids(&self) -> Vec<BundleId> {
        self.0
            .reviewer_bundle_ids
            .read()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Snapshot of Bundle IDs currently running an Integrator task.
    fn list_running_integrator_bundle_ids(&self) -> Vec<BundleId> {
        self.0
            .integrator_bundle_ids
            .read()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Reactive recovery for an `InProgress` Work whose Implementer is no
    /// longer live. Mirrors `override_work`'s shim pattern (shim ->
    /// work_spawner_tasks -> transition_and_persist_work) with two
    /// deliberate differences:
    ///
    /// 1. **`Role::Reactor`, not `Role::Director`.** The Work FSM's override
    ///    table allows `InProgress -> Ready` for Reactor only, by design;
    ///    this recovery is mechanical (sidecar map says no live Implementer),
    ///    not LLM judgment, so Reactor is the correct semantic role.
    /// 2. **No Implementer spawn after persist.** Unlike `override_work` (which
    ///    re-kicks the Implementer because the Director's primary use is
    ///    `Blocked -> Ready` retry), this recovery only flips the FSM. The
    ///    daemon's dep-gate watcher and the Director's next-iteration
    ///    `assign_work` together handle `Ready -> InProgress` re-promotion.
    ///    Per Phase 2 of the design doc, "the recovery does NOT itself spawn
    ///    an Implementer." Phase 3's integration test asserts the Ready
    ///    transition and `attempt_count` bump; re-promotion is a separate
    ///    concern owned by the reactive layer.
    fn recover_in_progress_work(&self, work_id: WorkId, reason: String) {
        let ctx_for_lock = Arc::clone(&self.0);
        let ctx = Arc::clone(&self.0);
        tokio::spawn(async move {
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let mut tasks = ctx_for_lock.work_spawner_tasks.lock().await;
            // Re-check under the lock: the shutdown drain holds this same
            // lock while draining, and `shutting_down` is set before any
            // drain runs, so a shim that passed the pre-lock check could
            // otherwise insert into an already-drained JoinSet
            // (check-then-lock race). Observing it true here means skip.
            if ctx_for_lock.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tasks.spawn(async move {
                if ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                    debug!(work_id = %work_id, "shutdown in progress; skipping recover_in_progress_work");
                    return;
                }
                let mut work = match ctx.store.works().get(&work_id).await {
                    Ok(w) => w,
                    Err(e) => {
                        warn!(error = %e, work_id = %work_id, "recover_in_progress_work: work lookup failed");
                        return;
                    }
                };
                if work.status != WorkStatus::InProgress {
                    debug!(
                        work_id = %work_id,
                        status = ?work.status,
                        "recover_in_progress_work: work no longer InProgress; skipping"
                    );
                    return;
                }
                info!(
                    work_id = %work_id,
                    attempt_count = work.attempt_count,
                    reason = %reason,
                    "recover_in_progress_work: applying Reactor-role InProgress -> Ready override"
                );
                if let Err(e) = transition_and_persist_work(
                    &*ctx.summary_fanout,
                    &mut work,
                    WorkStatus::Ready,
                    Role::Reactor,
                    true, // override
                    &ctx.snapshot,
                )
                .await
                {
                    warn!(error = %e, work_id = %work_id, "recover_in_progress_work: persist failed");
                }
            });
        });
    }
}
