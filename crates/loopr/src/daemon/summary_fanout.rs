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

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use domain::{Bundle, Plan, Work};
use store::{
    BundleUpdateError, BundleUpdateSink, PlanUpdateError, PlanUpdateSink, Store, WorkUpdateError, WorkUpdateSink,
};
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
///
/// `#[allow(dead_code)]` is load-bearing for Phase 5: the decorator
/// is introduced here and consumed at every transition site in Phase
/// 6 (`docs/design/2026-04-25-tier1-cleanup.md`). Phase 6 removes
/// these allowances. The crate's `#![deny(dead_code)]` would
/// otherwise block Phase 5 from shipping in isolation.
#[allow(dead_code)]
pub struct SummaryFanout<S> {
    inner: S,
    target: PathBuf,
    store: Arc<Store>,
}

impl<S> SummaryFanout<S> {
    #[allow(dead_code)]
    pub fn new(inner: S, target: PathBuf, store: Arc<Store>) -> Self {
        Self { inner, target, store }
    }
}

impl<S> WorkUpdateSink for SummaryFanout<S>
where
    S: WorkUpdateSink,
{
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(&'a self, work: Work) -> impl Future<Output = Result<(), WorkUpdateError>> + Send + 'a {
        async move {
            let work_for_summary = work.clone();
            self.inner.update(work).await?;
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
            Ok(())
        }
    }
}

impl<S> SummaryFanout<S> {
    #[allow(dead_code)]
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
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(
        &'a self,
        bundle: Bundle,
        expected_updated_at: i64,
    ) -> impl Future<Output = Result<(), BundleUpdateError>> + Send + 'a {
        async move {
            let bundle_for_summary = bundle.clone();
            self.inner.update(bundle, expected_updated_at).await?;
            if let Err(e) = summary::write_bundle(&self.target, &bundle_for_summary) {
                warn!(
                    bundle_id = %bundle_for_summary.id,
                    error = %e,
                    "summary::write_bundle failed (non-fatal)"
                );
            }
            Ok(())
        }
    }
}

impl<S> PlanUpdateSink for SummaryFanout<S>
where
    S: PlanUpdateSink,
{
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(
        &'a self,
        plan: Plan,
        children: Vec<Work>,
    ) -> impl Future<Output = Result<(), PlanUpdateError>> + Send + 'a {
        async move {
            let plan_for_summary = plan.clone();
            let children_for_summary = children.clone();
            self.inner.update(plan, children).await?;
            if let Err(e) = summary::write_plan(&self.target, &plan_for_summary, &children_for_summary) {
                warn!(
                    plan_id = %plan_for_summary.id,
                    error = %e,
                    "summary::write_plan failed (non-fatal)"
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
