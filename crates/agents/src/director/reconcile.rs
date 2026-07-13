//! Reconcile sweep: promotes `Integrated` Work to `Done`, recovers three
//! deterministic stuck-state cases, and reports GoalComplete. Pulled into
//! its own submodule (mirroring `mode.rs` / `pattern.rs`) to keep the
//! parent `director.rs` under the 1500-line bloat-task cap.

use std::collections::HashSet;

use domain::{BundleId, BundleStatus, PlanId, WorkId, WorkStatus, now_millis};
use tracing::{instrument, warn};

use super::{DirectorError, DirectorStore, WorkSpawner};

/// Reconcile sweep. Promotes any `Integrated` Work to `Done`, recovers
/// three deterministic stuck-state cases (Triaged-no-Reviewer, Accepted-no-
/// Integrator, InProgress-no-Implementer), and reports whether the Plan has
/// reached GoalComplete (every Work terminal AND at least one Done).
///
/// `grace_ms` is the per-record age threshold under which a stuck-state
/// recovery is skipped. The caller computes this from
/// `DirectorConfig::reconcile_grace_secs` (Phase 2 of
/// `docs/design/2026-05-09-director-phase-2.md`). The grace window absorbs
/// the spawn-chain race: a record that JUST entered its current status may
/// not yet have its sidecar-map entry populated; recovery on that record
/// would double-spawn.
#[instrument(
    name = "director.reconcile",
    level = "debug",
    skip_all,
    fields(
        plan_id = %plan_id,
        works_count = tracing::field::Empty,
        bundles_count = tracing::field::Empty,
        promoted_count = tracing::field::Empty,
        recovered_count = tracing::field::Empty,
        goal_complete = tracing::field::Empty,
    ),
    err,
)]
pub async fn reconcile_director<S: DirectorStore, P: WorkSpawner>(
    plan_id: &PlanId,
    store: &S,
    spawner: &P,
    grace_ms: i64,
) -> Result<bool, DirectorError> {
    let works = store.list_works_for_plan(plan_id).await?;
    let bundles = store.list_bundles_for_plan(plan_id).await?;
    let span = tracing::Span::current();
    span.record("works_count", works.len());
    span.record("bundles_count", bundles.len());

    if works.is_empty() {
        warn!(plan_id = %plan_id, "reconcile: zero works for plan; treating as not GoalComplete");
        span.record("promoted_count", 0u32);
        span.record("recovered_count", 0u32);
        span.record("goal_complete", false);
        return Ok(false);
    }

    let mut promoted = 0u32;
    for w in works.iter().filter(|w| w.status == WorkStatus::Integrated) {
        spawner.override_work(w.id.clone(), WorkStatus::Done, "reconcile: Integrated->Done".into());
        promoted += 1;
    }
    span.record("promoted_count", promoted);

    // Stuck-state recovery: three deterministic cases. Each guarded by the
    // grace window against `record.updated_at`. Sidecar-map snapshots come
    // from `WorkSpawner::list_running_*_ids`, which return the alive set;
    // a record whose ID is in that set has a live task and is not stuck.
    let now_ms = now_millis();
    let mut recovered: u32 = 0;

    // 1. Triaged Bundle without live Reviewer -> spawn_reviewer.
    let live_reviewer_bundles: HashSet<BundleId> = spawner.list_running_reviewer_bundle_ids().into_iter().collect();
    for b in bundles.iter().filter(|b| b.status == BundleStatus::Triaged) {
        let age_ms = now_ms - b.updated_at;
        if age_ms < grace_ms {
            continue;
        }
        if !live_reviewer_bundles.contains(&b.id) {
            warn!(
                bundle_id = %b.id,
                age_ms,
                "reconcile: Triaged Bundle with no live Reviewer; re-spawning"
            );
            spawner.spawn_reviewer(b.id.clone());
            recovered += 1;
        }
    }

    // 2. Accepted Bundle without live Integrator -> spawn_integrator.
    let live_integrator_bundles: HashSet<BundleId> = spawner.list_running_integrator_bundle_ids().into_iter().collect();
    for b in bundles.iter().filter(|b| b.status == BundleStatus::Accepted) {
        let age_ms = now_ms - b.updated_at;
        if age_ms < grace_ms {
            continue;
        }
        if !live_integrator_bundles.contains(&b.id) {
            warn!(
                bundle_id = %b.id,
                age_ms,
                "reconcile: Accepted Bundle with no live Integrator; spawning"
            );
            spawner.spawn_integrator(b.id.clone());
            recovered += 1;
        }
    }

    // 3. InProgress Work without live Implementer -> recover_in_progress_work.
    //    Reactive recovery, not Director judgment: the override table allows
    //    `InProgress -> Ready by (Reactor)` only. The production spawner
    //    impl applies the override under `Role::Reactor` and bumps
    //    `attempt_count` via the Layer-1 increment site in
    //    `transition_and_persist_work`, so repeated panics cycle through the
    //    same retry budget the Director's `Blocked -> Ready` retries consume.
    let live_work_ids: HashSet<WorkId> = spawner.list_running_work_ids().into_iter().collect();
    for w in works.iter().filter(|w| w.status == WorkStatus::InProgress) {
        let age_ms = now_ms - w.updated_at;
        if age_ms < grace_ms {
            continue;
        }
        if !live_work_ids.contains(&w.id) {
            warn!(
                work_id = %w.id,
                attempt_count = w.attempt_count,
                age_ms,
                "reconcile: InProgress Work with no live Implementer; transitioning to Ready"
            );
            spawner.recover_in_progress_work(w.id.clone(), "reconcile: InProgress with no live Implementer".into());
            recovered += 1;
        }
    }
    span.record("recovered_count", recovered);

    let all_terminal = works.iter().all(|w| w.status.is_terminal());
    let any_done = works.iter().any(|w| w.status == WorkStatus::Done);
    let goal_complete = all_terminal && any_done;
    span.record("goal_complete", goal_complete);
    Ok(goal_complete)
}
