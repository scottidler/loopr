//! Sibling-Work reactions to a Work reaching a terminal status: promote the
//! dep-unblocked Pending siblings (`promote_unblocked_siblings`) and block the
//! transitive dependents of an irrecoverable one (`block_dependent_siblings`).
//! Extracted from the parent `context` module to keep `context.rs` under the
//! per-file line limit (same pattern as `spawner.rs` / `integration.rs` /
//! `reap.rs`), and re-exported from `context` so `super::` / module-path
//! callers (integration.rs, startup.rs) keep resolving unchanged.
//!
//! Both return a boxed future (not `impl Future`) to break the E0391
//! opaque-type cycle through the spawn call graph — see the per-fn docs.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{Instrument, debug, info, warn};

use domain::{PlanId, Role, Work, WorkGraph, WorkId, WorkStatus};
use llm::LlmClient;

use super::{DaemonContext, transition_and_persist_work};

/// Scan Pending sibling Works for the given Plan and spawn an
/// Implementer for any whose deps are now all Done.
///
/// Returns `Pin<Box<dyn Future<...>>>` (not `impl Future`) so rustc can
/// resolve the return type without following the async call graph into
/// `spawn_implementer_for_work` -> `spawn_reviewer_for_bundle` ->
/// `spawn_integrator_for_bundle` -> this function (E0391 cycle). A
/// concrete boxed-future return type breaks the opaque-type cycle at
/// this edge.
///
/// Called after every `Integrated -> Done` transition and during
/// startup reconcile (crash-recovery gap). Best-effort: store errors
/// are logged and dropped so a sibling-sweep failure never kills the
/// caller's success path.
pub(crate) fn promote_unblocked_siblings<L: LlmClient + Send + Sync + 'static>(
    ctx: Arc<DaemonContext<L>>,
    plan_id: PlanId,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
    let span = tracing::info_span!("daemon.promote_unblocked_siblings", plan_id = %plan_id);
    Box::pin(
        async move {
            let siblings = match ctx.store.works().list_by_parent_id(&plan_id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "promote_unblocked_siblings: list_by_parent_id failed");
                    return;
                }
            };
            let graph = WorkGraph::from_works(&siblings);
            let done: HashSet<WorkId> = siblings
                .iter()
                .filter(|w| w.status == WorkStatus::Done)
                .map(|w| w.id.clone())
                .collect();
            let ready: HashSet<WorkId> = graph.ready_set(&done).into_iter().collect();
            let pending_ready: Vec<Work> = siblings
                .into_iter()
                .filter(|w| w.status == WorkStatus::Pending && ready.contains(&w.id))
                .collect();
            // Pre-spawn drain guard: this runs as the continuation of an
            // integrator completing (`Integrated -> Done` promotes siblings).
            // If a shutdown signal landed while that integrator ran, the
            // implementer pool may already be draining; spawning into it
            // would strand an `Arc<DaemonContext>` clone and defeat the
            // shutdown `Arc::try_unwrap`. Skip promotion entirely, and the
            // pool drain then returns cleanly. The `shutting_down` guard at
            // the top of `spawn_implementer_for_work` is the belt to this
            // suspenders (covers the check-then-spawn race under the lock).
            if ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                debug!("shutdown in progress; skipping sibling promotion");
                return;
            }
            let promoted = pending_ready.len();
            for work in pending_ready {
                ctx.spawn_implementer_registered(work).await;
            }
            info!(promoted, "promote_unblocked_siblings: done");
        }
        .instrument(span),
    )
}

/// Mark any Pending Works whose `dependencies` contains `terminal_work_id`
/// as `Blocked`, writing `blocked_reason` to explain that a dep became
/// irrecoverable.
///
/// Only called when `terminal_work_id` reaches `Abandoned` or `Superseded`
/// (truly terminal, non-Done). `Blocked` deps are excluded because they
/// may still recover via 1.3's recovery loop.
///
/// Returns `Pin<Box<dyn Future<...>>>` for the same E0391 reason as
/// `promote_unblocked_siblings`.
pub(crate) fn block_dependent_siblings<L: LlmClient + Send + Sync + 'static>(
    ctx: Arc<DaemonContext<L>>,
    plan_id: PlanId,
    terminal_work_id: WorkId,
    terminal_status: WorkStatus,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
    let span = tracing::warn_span!(
        "daemon.block_dependent_siblings",
        plan_id = %plan_id,
        terminal_work_id = %terminal_work_id,
        terminal_status = ?terminal_status,
    );
    Box::pin(
        async move {
            let siblings = match ctx.store.works().list_by_parent_id(&plan_id).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "block_dependent_siblings: list_by_parent_id failed");
                    return;
                }
            };
            let graph = WorkGraph::from_works(&siblings);
            // F7: block the FULL transitive closure of dependents, not just
            // the direct ones. A Work that depends (even transitively) on a
            // terminal Work can never have all its deps reach Done, so it is
            // irrecoverable. BFS over the reverse-dependency edges from the
            // terminal Work to a fixpoint; the graph topology is static, so a
            // single closure pass IS the fixpoint (no re-listing needed).
            let mut closure: HashSet<WorkId> = HashSet::new();
            let mut frontier: Vec<WorkId> = graph.dependents_of(&terminal_work_id).to_vec();
            while let Some(node) = frontier.pop() {
                if closure.insert(node.clone()) {
                    frontier.extend(graph.dependents_of(&node).iter().cloned());
                }
            }
            let pending_dependents: Vec<Work> = siblings
                .iter()
                .filter(|w| w.status == WorkStatus::Pending && closure.contains(&w.id))
                .cloned()
                .collect();
            let mut blocked = 0usize;
            for mut work in pending_dependents {
                work.blocked_reason = Some(format!(
                    "dep {} reached {:?}; irrecoverable",
                    terminal_work_id, terminal_status
                ));
                if let Err(e) = transition_and_persist_work(
                    &*ctx.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    false,
                    &ctx.snapshot,
                )
                .await
                {
                    warn!(work_id = %work.id, error = %e, "block_dependent_siblings: Pending -> Blocked failed");
                } else {
                    blocked += 1;
                }
            }
            warn!(blocked, "block_dependent_siblings: done");
        }
        .instrument(span),
    )
}
