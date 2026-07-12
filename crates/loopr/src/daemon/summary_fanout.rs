//! `SummaryFanout<S>`: per-transition summary-write decorator.
//!
//! Phase 5 of the Tier-1 cleanup. Wraps an inner sink and the target
//! path, implementing all three sink traits (`WorkUpdateSink`,
//! `BundleUpdateSink`, `PlanUpdateSink`) when the inner type does. On
//! every successful inner update, the decorator writes the matching
//! per-record summary file under
//! `<target>/.loopr/records/<kind>/<id>/summary.md`.
//!
//! Per design Alternatives §4 option (c-extended), the
//! `WorkUpdateSink` impl additionally re-renders the parent Plan
//! summary on every Work transition: a Work whose `parent_id` resolves
//! to a Plan triggers a `summary::write_plan(target, plan, &siblings)`
//! call so the on-disk Plan summary reflects the new child state.
//! That requires the decorator to hold an `Arc<Store>` for the
//! parent-Plan + siblings reads; the trait-purity of plain option (c)
//! is preserved at the `PlanUpdateSink` boundary.
//!
//! A summary-write failure emits `warn!` and the impl returns
//! `Ok(())` so a per-summary error never rolls back a successful FSM
//! transition (or its OCC version bump). Likewise, missing-or-non-Plan
//! parents on the c-extended Work-update path emit `debug!` and skip
//! silently — Plan-summary refresh is best-effort.

use std::path::PathBuf;
use std::sync::Arc;

use domain::{
    Bundle, BundleId, CheckRun, CheckRunId, Plan, PlanStatus, Review, ReviewId, Role, TargetKind, Work, WorkStatus,
};
use ipc::DaemonEvent;
use store::{
    BundleUpdateError, BundleUpdateSink, CheckRunSink, PlanUpdateError, PlanUpdateSink, ReviewSink, Store, StoreError,
    WorkUpdateError, WorkUpdateSink,
};
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::summary;

/// Per-transition summary-write decorator. Constructed once at daemon
/// boot and passed into every `transition_and_persist_*` call site.
///
/// The `store` field is shared with the daemon for the c-extended
/// Work-update path's parent-Plan + siblings reads. `inner` is the
/// underlying sink (also typically the same `Arc<Store>` in
/// production; tests inject `MockStore` fakes). The split exists so
/// the decorator can swap test fakes per-trait without re-coupling
/// every impl to the concrete `Store` type.
pub struct SummaryFanout<S> {
    inner: S,
    target: PathBuf,
    store: Arc<Store>,
    /// Broadcast bus for `DaemonEvent`s. `None` in tests (the `new`
    /// constructor); production wires the daemon's `events` sender via
    /// `with_events` so terminal/Blocked Work transitions and
    /// terminal/Stalled Plan transitions reach connected clients. The bus
    /// previously had a single sender (`budget.exceeded`); this is the
    /// other documented sender the vision promised.
    events: Option<broadcast::Sender<DaemonEvent>>,
}

impl<S> SummaryFanout<S> {
    /// Test/standalone constructor: no event bus wired.
    pub fn new(inner: S, target: PathBuf, store: Arc<Store>) -> Self {
        Self {
            inner,
            target,
            store,
            events: None,
        }
    }

    /// Production constructor: wires the daemon's `DaemonEvent` broadcast
    /// sender so lifecycle transitions emit on the bus.
    pub fn with_events(inner: S, target: PathBuf, store: Arc<Store>, events: broadcast::Sender<DaemonEvent>) -> Self {
        Self {
            inner,
            target,
            store,
            events: Some(events),
        }
    }

    /// Emit a `DaemonEvent` on the bus if one is wired. Best-effort: a send
    /// error (no subscribers) is swallowed — events are advisory.
    fn emit(&self, event: &str, data: serde_json::Value) {
        if let Some(tx) = &self.events {
            let _ = tx.send(DaemonEvent {
                event: event.to_string(),
                data,
            });
        }
    }

    /// Emit a Work lifecycle event on a terminal or Blocked transition.
    fn emit_work_event(&self, work: &Work) {
        let event = if work.status == WorkStatus::Blocked {
            "work.blocked"
        } else if work.status.is_terminal() {
            "work.terminal"
        } else {
            return;
        };
        self.emit(
            event,
            serde_json::json!({
                "work_id": work.id.to_string(),
                "plan_id": work.parent_id.to_string(),
                "status": format!("{:?}", work.status),
                "blocked_reason": work.blocked_reason,
                "failure_reason": work.failure_reason.as_ref().map(|r| format!("{r:?}")),
            }),
        );
    }

    /// Emit a Plan lifecycle event on a terminal or Stalled transition.
    fn emit_plan_event(&self, plan: &Plan) {
        let event = if plan.status == PlanStatus::Stalled {
            "plan.stalled"
        } else if plan.status.is_terminal() {
            "plan.terminal"
        } else {
            return;
        };
        self.emit(
            event,
            serde_json::json!({
                "plan_id": plan.id.to_string(),
                "status": format!("{:?}", plan.status),
            }),
        );
    }
}

impl<S> WorkUpdateSink for SummaryFanout<S>
where
    S: WorkUpdateSink,
{
    async fn update(
        &self,
        mut work: Work,
        expected_updated_at: i64,
        role: Role,
        kind: TargetKind,
    ) -> Result<i64, WorkUpdateError> {
        let persisted = self.inner.update(work.clone(), expected_updated_at, role, kind).await?;
        // Reflect the persisted (floored) `updated_at` in the summary so
        // the on-disk record and its summary agree.
        work.updated_at = persisted;
        let work_for_summary = work;
        // Best-effort: write the Work summary first.
        if let Err(e) = summary::write_work(&self.target, &work_for_summary) {
            warn!(
                work_id = %work_for_summary.id,
                error = %e,
                "summary::write_work failed (non-fatal)"
            );
        }
        // C-extended: refresh the parent Plan's summary so it
        // reflects this Work transition's new status counts.
        // `parent_id` may be a Plan (Phase-1 simple shape) or a
        // Spec/Phase (Tier-2 multi-tier shape, not built); the
        // Plan-resolve is itself best-effort.
        self.refresh_parent_plan(&work_for_summary).await;
        // Surface terminal/Blocked transitions on the DaemonEvent bus.
        self.emit_work_event(&work_for_summary);
        Ok(persisted)
    }
}

impl<S> SummaryFanout<S> {
    async fn refresh_parent_plan(&self, work: &Work) {
        // `Work::parent_id` is typed as `PlanId` today; the resolve is
        // therefore expected to succeed when the Work is well-formed.
        // Future readers: if the parent-id type widens to a sum of
        // PlanId/SpecId/PhaseId, this is the call site that has to
        // dispatch on the variant.
        let plan_id = &work.parent_id;
        let plan = match self.store.plans().get(plan_id).await {
            Ok(p) => p,
            Err(e) => {
                debug!(
                    parent_id = %plan_id,
                    error = %e,
                    "summary_fanout: parent Plan not resolvable; skipping Plan-summary refresh"
                );
                return;
            }
        };
        let children = match self.store.works().list_by_parent_id(plan_id).await {
            Ok(c) => c,
            Err(e) => {
                debug!(
                    plan_id = %plan_id,
                    error = %e,
                    "summary_fanout: list_by_parent_id failed; skipping Plan-summary refresh"
                );
                return;
            }
        };
        if let Err(e) = summary::write_plan(&self.target, &plan, &children) {
            warn!(
                plan_id = %plan_id,
                error = %e,
                "summary::write_plan failed during Work-transition fanout (non-fatal)"
            );
        }
    }
}

impl<S> BundleUpdateSink for SummaryFanout<S>
where
    S: BundleUpdateSink,
{
    async fn update(
        &self,
        mut bundle: Bundle,
        expected_updated_at: i64,
        role: Role,
        kind: TargetKind,
    ) -> Result<i64, BundleUpdateError> {
        let persisted = self
            .inner
            .update(bundle.clone(), expected_updated_at, role, kind)
            .await?;
        bundle.updated_at = persisted;
        let bundle_for_summary = bundle;
        if let Err(e) = summary::write_bundle(&self.target, &bundle_for_summary) {
            warn!(
                bundle_id = %bundle_for_summary.id,
                error = %e,
                "summary::write_bundle failed (non-fatal)"
            );
        }
        Ok(persisted)
    }
}

/// CheckRun persistence flows through the decorator so `run_reviewer` (Phase
/// 10) can persist executed-check evidence via its single `store` handle. No
/// summary artifact for CheckRuns (they are append-only evidence, not a
/// summarized record); the impl forwards straight to the inner `Store`'s
/// `check_runs` collection. `S: Send + Sync` is all that's required — the
/// inner sink type is irrelevant since CheckRuns write to `self.store`.
impl<S> CheckRunSink for SummaryFanout<S>
where
    S: Send + Sync,
{
    async fn create_check_run(&self, check_run: CheckRun) -> Result<CheckRunId, StoreError> {
        self.store.check_runs().create(check_run).await
    }
}

/// `ReviewSink` (Phase 11): like `CheckRunSink`, there is no summary artifact
/// for Reviews (append-only evidence, not a summarized record); the impl
/// forwards straight to the inner `Store`'s `reviews` collection. The Reviewer
/// persists one `Review` per round through this and reads prior rounds to
/// compute the next round number.
impl<S> ReviewSink for SummaryFanout<S>
where
    S: Send + Sync,
{
    async fn create_review(&self, review: Review) -> Result<ReviewId, StoreError> {
        self.store.reviews().create(review).await
    }

    async fn list_reviews_by_bundle(&self, bundle_id: &BundleId) -> Result<Vec<Review>, StoreError> {
        self.store.reviews().list_by_bundle(bundle_id).await
    }
}

impl<S> PlanUpdateSink for SummaryFanout<S>
where
    S: PlanUpdateSink,
{
    async fn update(
        &self,
        mut plan: Plan,
        children: Vec<Work>,
        expected_updated_at: i64,
        role: Role,
        kind: TargetKind,
    ) -> Result<i64, PlanUpdateError> {
        let children_for_summary = children.clone();
        let persisted = self
            .inner
            .update(plan.clone(), children, expected_updated_at, role, kind)
            .await?;
        plan.updated_at = persisted;
        let plan_for_summary = plan;
        if let Err(e) = summary::write_plan(&self.target, &plan_for_summary, &children_for_summary) {
            warn!(
                plan_id = %plan_for_summary.id,
                error = %e,
                "summary::write_plan failed (non-fatal)"
            );
        }
        // Surface terminal/Stalled transitions on the DaemonEvent bus.
        self.emit_plan_event(&plan_for_summary);
        Ok(persisted)
    }
}

#[cfg(test)]
mod tests;
