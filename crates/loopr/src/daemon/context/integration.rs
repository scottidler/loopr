//! `DaemonContext::spawn_integrator_for_bundle`, extracted from
//! `context.rs` so that file stays under the per-file line limit (the
//! same rationale as `spawner.rs` next door). This is an inherent-impl
//! method on `DaemonContext`; Rust allows the impl block to live in a
//! child module of the one that defines the type.

use std::sync::Arc;

use domain::{Bundle, PlanStatus, Role, WorkStatus};
use integrator::{IntegrationError, IntegratorDeps, integrate};
use llm::LlmClient;
use store::BundleUpdateError;
use tracing::{debug, error, info, instrument, warn};

use super::{
    DaemonContext, INTEGRATOR_BACKOFF, ScopedIdGuard, compute_plan_summary_extras, promote_unblocked_siblings,
    transition_and_persist_plan, transition_and_persist_work,
};

impl<L: LlmClient + Send + Sync + 'static> DaemonContext<L> {
    /// Integrate an Accepted Bundle onto the Plan's integration branch
    /// and produce a Tick. Retries on transient errors with the
    /// `INTEGRATOR_BACKOFF` schedule, capped at 5 attempts total.
    ///
    /// Integrator doc contract: a Bundle at `Integrating` on `integrate`
    /// return is NOT a terminal failure; the daemon re-enqueues it.
    /// This method honors that by treating `Update(Stale)` and `Store`
    /// errors as retryable, and any other IntegrationError variant as
    /// terminal (no retry; Work -> Blocked).
    ///
    /// Shutdown-aware: shutdown_notify cuts the backoff sleep so a Ctrl-C
    /// during a retry does not block the daemon for 12.6s.
    #[tracing::instrument(level = "info", skip_all, fields(bundle_id = %bundle.id, work_id = %bundle.work_id, session_id = %self.session_id))]
    #[instrument(
        name = "daemon.spawn_integrator_for_bundle",
        level = "info",
        skip_all,
        fields(bundle_id = %bundle.id, work_id = %bundle.work_id),
    )]
    pub async fn spawn_integrator_for_bundle(self: Arc<Self>, bundle: Bundle) {
        // Phase 2 sidecar-map insert; mirrors the implementer/reviewer wrappers.
        let _id_guard = ScopedIdGuard::new(Arc::clone(&self.integrator_bundle_ids), bundle.id.clone());

        if self.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
            debug!("shutdown in progress; skipping integrator spawn");
            return;
        }

        // Load Work + Plan. Both failures are non-retryable (records
        // fundamentally missing), so we log and return.
        let mut work = match self.store.works().get(&bundle.work_id).await {
            Ok(w) => w,
            Err(e) => {
                error!(error = %e, "work lookup failed during integrate; skipping");
                return;
            }
        };
        let plan = match self.store.plans().get(&work.parent_id).await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "plan lookup failed during integrate; skipping");
                return;
            }
        };

        let deps = IntegratorDeps {
            // Phase 6: BundleUpdateSink goes through the fanout so the
            // Integrator's `Reviewed -> Merged` write produces an
            // up-to-date Bundle summary in lockstep with the OCC
            // update. `works` and `ticks` are read paths (resolved on
            // `Store` directly) and stay on the underlying store.
            bundle_sink: &*self.summary_fanout,
            works: &*self.store,
            ticks: &*self.store,
            config: self.integrator_config.clone(),
            target: self.target.clone(),
            git_lock: Arc::clone(&self.git_lock),
        };

        // Retry loop with circuit breaker. `attempt` is 0-indexed into
        // INTEGRATOR_BACKOFF; each iteration either integrates or sleeps
        // the corresponding backoff then tries again.
        let outcome: Result<domain::Tick, IntegrationError> = 'retry: {
            for (attempt, &backoff) in INTEGRATOR_BACKOFF.iter().enumerate() {
                match integrate(std::slice::from_ref(&bundle), &plan, &deps).await {
                    Ok(tick) => break 'retry Ok(tick),
                    Err(IntegrationError::Update(BundleUpdateError::Stale { .. }))
                    | Err(IntegrationError::Store(_))
                        if attempt + 1 < INTEGRATOR_BACKOFF.len() =>
                    {
                        warn!(
                            attempt = attempt + 1,
                            total_attempts = INTEGRATOR_BACKOFF.len(),
                            backoff_ms = backoff.as_millis(),
                            "integrator retryable error; backing off"
                        );
                        // Respect shutdown during backoff; select against
                        // the notify waker so a SIGTERM does not block.
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = self.shutdown_notify.notified() => {
                                warn!("shutdown during integrator backoff; abandoning retry");
                                return;
                            }
                        }
                    }
                    Err(e) => break 'retry Err(e),
                }
            }
            // Fell off the end of the schedule with all attempts retryable.
            // Circuit-break.
            Err(IntegrationError::Git(
                "integrator circuit breaker tripped: 5 retryable-error attempts exhausted".into(),
            ))
        };

        match outcome {
            Ok(tick) => {
                info!(tick_id = %tick.id, sha = %tick.sha, "integration succeeded");
                // Phase 4: this Tick-persist site is the loopr-side
                // observation point for both dead counters. `integrate()`
                // was called with a single-Bundle slice
                // (`std::slice::from_ref(&bundle)`), so `Ok(tick)` means
                // exactly the one `bundle` in scope here got merged to
                // produce it.
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.ticks_created += 1;
                    snap.bundles_merged += 1;
                } else {
                    warn!("spawn_integrator_for_bundle: snapshot Mutex poisoned; ticks_created/bundles_merged dropped");
                }
                // Work: InReview -> Integrated -> Done.
                if let Err(e) = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Integrated,
                    Role::Integrator,
                    false,
                    &self.snapshot,
                )
                .await
                {
                    error!(error = %e, "InReview -> Integrated transition failed after Tick persisted");
                    return;
                }
                if let Err(e) = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Done,
                    Role::Reactor,
                    false,
                    &self.snapshot,
                )
                .await
                {
                    error!(error = %e, "Integrated -> Done transition failed after Tick persisted");
                    return;
                }
                // Dep gate: promote any Pending siblings whose deps are
                // now all Done. Best-effort; failure is already logged
                // inside promote_unblocked_siblings.
                {
                    let ctx = Arc::clone(&self);
                    promote_unblocked_siblings(ctx, plan.id.clone()).await;
                }

                // Plan-level completion check: if every sibling Work is
                // terminal with at least one Done, fire Plan:
                // Active -> Complete. Best-effort; log + continue on Err.
                //
                // F1: re-fetch the Plan fresh immediately before the
                // transition rather than reusing the snapshot loaded at
                // the top of this method (minutes ago, before the
                // integrate). A concurrent Director/IPC write since then
                // would otherwise make the OCC `expected_updated_at`
                // stale and reject the Complete transition spuriously.
                let mut plan_mut = match self.store.plans().get(&plan.id).await {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(error = %e, "plan re-fetch before completion check failed; skipping");
                        return;
                    }
                };
                if let Ok(siblings) = self.store.works().list_by_parent_id(&plan_mut.id).await {
                    let all_terminal = !siblings.is_empty() && siblings.iter().all(|w| w.status.is_terminal());
                    let any_done = siblings.iter().any(|w| w.status == WorkStatus::Done);
                    if all_terminal && any_done {
                        // Phase 8: compute Plan-level summary extras
                        // (ticks + bundle terminal counts) from the
                        // store. Best-effort: a query failure leaves
                        // the field at 0 rather than failing the
                        // Plan transition.
                        let extras = compute_plan_summary_extras(&self.store, &plan_mut.id, &siblings).await;
                        // Phase 6: c-extended (option c) — pass siblings as
                        // the children arg so SummaryFanout's PlanUpdateSink
                        // impl can render the Plan summary against the
                        // current child set without a separate read.
                        match transition_and_persist_plan(
                            &*self.summary_fanout,
                            &mut plan_mut,
                            siblings,
                            PlanStatus::Complete,
                            Role::Reactor,
                            extras,
                            false,
                        )
                        .await
                        {
                            Ok(()) => info!(plan_id = %plan_mut.id, "plan Active -> Complete"),
                            Err(e) => warn!(error = %e, "plan Active -> Complete transition failed (non-fatal)"),
                        }
                    }
                }
                // Per-record summaries are now written transactionally
                // by SummaryFanout inside each transition's `update`
                // call. The post-Integrator inline `write_*_summary_best_effort`
                // helpers (and the post-fetch reads used to build them)
                // are gone — kept as a comment for the historical record.
                let _ = bundle; // bundle was previously re-fetched here for the inline summary
                let _ = work;
            }
            Err(IntegrationError::ValidationFailed {
                ref command, exit_code, ..
            }) => {
                // Bundles are already IntegrationFailed (integrate() called
                // fail_all_without_reset before returning). Only Work needs
                // a state change here.
                warn!(
                    command = %command,
                    exit_code = ?exit_code,
                    "post-merge validation failed; marking Work Blocked"
                );
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                    &self.snapshot,
                )
                .await;
                self.wake_director(&work.parent_id).await;
            }
            Err(e) => {
                error!(error = %e, "integrator terminal; marking Work Blocked");
                // One-step via the Phase 1 InReview -> Blocked override.
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                    &self.snapshot,
                )
                .await;
                self.wake_director(&work.parent_id).await;
            }
        }
    }
}
