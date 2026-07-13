//! Typed FSM-transition-and-persist helpers for the daemon's record kinds
//! (`Work`, `Bundle`, `Plan`). Extracted from the parent `context` module to
//! keep `context.rs` under the per-file line limit (same pattern as
//! `spawner.rs` / `integration.rs` / `reap.rs`), and re-exported from
//! `context` so every existing `crate::daemon::context::X` / `super::X` import
//! path (integration.rs, spawner.rs, transport/handler.rs, startup.rs, and the
//! crate's integration tests) keeps resolving unchanged.
//!
//! Each helper snapshots the OCC token, runs the FSM edge, skips the store
//! write on `Transition::Unchanged`, and re-syncs the in-memory record's
//! `updated_at` from the floored value the store returns — the invariant that
//! `docs/design/2026-07-12-reviewer-occ-stale-race.md` defends for Bundles and
//! that Work/Plan already had.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tracing::{debug, error, info, warn};

use domain::{Bundle, BundleStatus, Plan, PlanId, PlanStatus, Role, Work, WorkStatus};
use store::Store;
use telemetry::digest::process::ProcessSnapshot;

/// Spawner-layer hard cap on `Work.attempt_count` — defense-in-depth
/// circuit breaker. The Director-layer soft cap
/// (`DirectorConfig.max_work_attempts`, default 3) is the well-behaved
/// exit path; this constant catches retry paths that bypass the soft cap
/// (rogue caller, manual CLI intervention, future bug) and push a Work
/// to Ready a hundred-plus times. Set far above any plausible
/// operator-tunable soft cap.
pub const MAX_WORK_ATTEMPTS_HARD_CAP: u32 = 100;

/// Typed failure of `transition_and_persist_work`. Replaces the prior
/// `String` so callers can distinguish a benign OCC lost-race (`Stale`)
/// from a hard failure — `override_work` must NOT spawn an Implementer
/// when the persist lost the race (the persisted state belongs to the
/// racing winner), and the reviewer/spawner paths treat `Stale` as a
/// no-op rather than forcing the Work to `Blocked`.
#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    /// The FSM rejected the (override) transition.
    #[error("fsm rejected: {0}")]
    Fsm(String),
    /// Layer-3 hard cap refused the persist (attempt_count at the cap).
    #[error("work {work_id} attempt_count={attempt_count} hit MAX_WORK_ATTEMPTS_HARD_CAP={cap}; refusing persist")]
    HardCap {
        work_id: String,
        attempt_count: u32,
        cap: u32,
    },
    /// OCC version-check lost the race — benign; the Work was already
    /// advanced by another writer. Callers should return without
    /// clobbering rather than treat it as a hard failure.
    #[error("stale work: expected updated_at={expected}, actual={actual}")]
    Stale { expected: i64, actual: i64 },
    /// Underlying store write failed (non-OCC).
    #[error("works().update: {0}")]
    Persist(String),
}

/// Transition a Work via the FSM (transition or override) and persist via
/// `WorksStore::update`. Returns a typed `TransitionError` if the FSM
/// rejects the transition or if persistence fails; the caller logs and
/// decides whether to continue (matching `Stale` for benign races).
///
/// Stage 8 wiring capstone replaced the Stage 7 `mark_blocked` function,
/// which mutated `work.status = ...` by raw assignment and bypassed the
/// FSM entirely. Every Work-state change in the pipeline flows through
/// this helper.
///
/// **Retry-budget instrumentation (Director Phase 1 follow-ups, Layers 1
/// and 3).** On any successful transition where the new status is `Ready`,
/// `work.attempt_count` increments by 1 BEFORE the persist write, so a
/// Work that has run once has `attempt_count == 1` (1-based). Layer 3's
/// hard cap pre-checks the count BEFORE the increment; the persist is
/// refused if `attempt_count >= MAX_WORK_ATTEMPTS_HARD_CAP`.
///
/// **Pipeline counters (Phase 4 of the verified-swarm doc).** On a
/// persisted transition into a terminal `Work` status with a dedicated
/// `ProcessSnapshot` field, the matching counter increments:
/// `WorkStatus::Done` -> `works_completed`, `WorkStatus::Blocked` ->
/// `works_blocked`. `Abandoned`/`Superseded` are terminal too but have no
/// counter field yet, so they are not counted. Every call site threads its
/// own `&self.snapshot` / `&ctx.snapshot`, so the counters stay live
/// regardless of which caller drives the transition.
pub async fn transition_and_persist_work<S>(
    sink: &S,
    work: &mut Work,
    target: WorkStatus,
    role: Role,
    override_: bool,
    snapshot: &Arc<StdMutex<ProcessSnapshot>>,
) -> Result<(), TransitionError>
where
    S: store::WorkUpdateSink,
{
    let expected_updated_at = work.updated_at;
    let result = if override_ {
        work.override_status(target, role)
            .map_err(|e| TransitionError::Fsm(format!("override: {e}")))?
    } else {
        work.transition(target, role)
            .map_err(|e| TransitionError::Fsm(format!("transition: {e}")))?
    };
    if result == domain::Transition::Unchanged {
        return Ok(());
    }

    // Layer 3 hard cap: refuse persist when attempt_count is already at
    // the hard cap. Pre-increment (>=) check keeps the cap strict — the
    // current attempt would be the (HARD_CAP+1)th if it landed.
    //
    // Order note: the design doc's Phase 4 prose puts this check
    // *after* Layer 1's increment. Implementing in that order requires
    // a `>` comparison against `HARD_CAP+1` (the post-increment value)
    // to fire on the same attempt; the as-implemented "check before
    // increment with `>=`" is the same boundary expressed without the
    // off-by-one. Documented here so a future reader doesn't try to
    // "fix" it back to the spec's literal sequence.
    if matches!(target, WorkStatus::Ready) && work.attempt_count >= MAX_WORK_ATTEMPTS_HARD_CAP {
        return Err(TransitionError::HardCap {
            work_id: work.id.to_string(),
            attempt_count: work.attempt_count,
            cap: MAX_WORK_ATTEMPTS_HARD_CAP,
        });
    }

    // Layer 1 increment: bump the cross-iteration retry counter on any
    // path to Ready. Fires for both the initial Pending->Ready dispatch
    // (first attempt) and Director-issued Blocked->Ready retries.
    if matches!(target, WorkStatus::Ready) {
        work.attempt_count = work.attempt_count.saturating_add(1);
    }

    // Sync the in-memory Work to the persisted (monotonically-floored)
    // `updated_at` so a chained next transition on the same record (e.g.
    // Integrated -> Done in `spawn_integrator_for_bundle`) carries the
    // correct OCC expected-version even when both writes land in the same
    // millisecond.
    // Phase 9: hand the store the same FSM intent this helper just used
    // (`override_status` vs `transition`) so the chokepoint re-validates
    // against the matching table. Behavior-neutral: the edge the store
    // re-checks is the one already accepted above.
    let kind = if override_ {
        domain::TargetKind::Override
    } else {
        domain::TargetKind::Normal
    };
    let persisted = match sink.update(work.clone(), expected_updated_at, role, kind).await {
        Ok(ts) => ts,
        Err(store::WorkUpdateError::Stale { expected, actual }) => {
            return Err(TransitionError::Stale { expected, actual });
        }
        Err(store::WorkUpdateError::Update(s)) => return Err(TransitionError::Persist(s)),
    };
    work.updated_at = persisted;

    // Phase 8: per-Work terminal summary. The richer metrics
    // (total_iterations, lifeguard_fires, director_override_count) are
    // not yet aggregated daemon-side; this event opens the door so a
    // future commit can extend the field set without changing the
    // event name. For now: work_id + terminal_state + role + override
    // is enough to grep "every Work that reached terminal in this run."
    if work.status.is_terminal() {
        info!(
            work_id = %work.id,
            plan_id = %work.parent_id,
            terminal_state = ?work.status,
            role = ?role,
            override_,
            attempt_count = work.attempt_count,
            session_failure_count = work.session_failure_count,
            "work: terminal-state summary"
        );
    }
    // Phase 4: wire the dead `works_completed` / `works_blocked` counters.
    // NOT nested inside the `is_terminal()` block above: `Blocked` has
    // outgoing FSM edges (`Blocked => Ready/Superseded/Abandoned`, see
    // `domain::Work`'s FSM table), so `work.status.is_terminal()` is
    // `false` for it — a Work can recover from Blocked, which is exactly
    // why the terminal-state summary log correctly excludes it. The
    // pipeline counter still needs to count every time a Work lands on
    // Blocked, terminal or not.
    if let Ok(mut snap) = snapshot.lock() {
        match work.status {
            WorkStatus::Done => snap.works_completed += 1,
            WorkStatus::Blocked => snap.works_blocked += 1,
            _ => {}
        }
    } else {
        warn!("transition_and_persist_work: snapshot Mutex poisoned; pipeline counters dropped");
    }
    Ok(())
}

/// Typed failure of `transition_and_persist_bundle`. Bundle-specific and
/// deliberately NOT a reuse of `TransitionError`: that enum's messages
/// read "stale work" / "works().update", and a log line calling a Bundle
/// a "work" is a names-tell-the-truth violation. `Stale` stays its own
/// variant so callers keep the existing exit-cleanly-on-lost-race
/// behavior (see docs/design/2026-07-12-reviewer-occ-stale-race.md).
#[derive(Debug, thiserror::Error)]
pub enum BundleTransitionError {
    /// The FSM rejected the transition.
    #[error("fsm rejected: {0}")]
    Fsm(String),
    /// OCC version-check lost the race — benign; the Bundle was already
    /// advanced by another writer. Callers exit cleanly rather than treat
    /// this as a hard failure.
    #[error("stale bundle: expected updated_at={expected}, actual={actual}")]
    Stale { expected: i64, actual: i64 },
    /// Underlying store write failed (non-OCC).
    #[error("bundles().update: {0}")]
    Persist(String),
}

/// Transition a Bundle via the FSM and persist through a
/// `BundleUpdateSink`. Mirrors `transition_and_persist_work`'s shape:
/// snapshot the OCC token, transition, and skip the store write entirely
/// on `Transition::Unchanged`. Every daemon call site that mutates a
/// Bundle's status must go through this helper — no hand-rolled
/// snapshot/transition/`.bundles().update` sequence, which is exactly the
/// class of bug this helper closes (docs/design/2026-07-12-reviewer-occ-
/// stale-race.md).
///
/// **Root cause this fixes.** The prior hand-rolled call sites discarded
/// the store's returned floored `updated_at`, so the in-memory Bundle
/// staled itself against its own very next write (a same-millisecond
/// triage + review lands the floor at `current + 1`, and the caller's
/// stale copy loses the following OCC check). This helper re-syncs
/// `bundle.updated_at` from the persisted value on every `Changed`
/// transition, so a chained next transition on the same in-memory Bundle
/// carries the correct expected version.
///
/// **Unchanged-skip.** On `Transition::Unchanged` (the target status
/// equals the current status — e.g. a reconcile-driven re-triage of an
/// already-Triaged Bundle) the store write is skipped entirely. An
/// unconditional write on a no-op transition floors disk to "now" and
/// resets the reconcile sweep's re-spawn age clock — that reset is
/// exactly the doom-loop mechanism this design doc removes. Callers must
/// not fall back to touching `updated_at` without a status change; no
/// such "status-preserving touch" path exists here by design.
pub async fn transition_and_persist_bundle<S>(
    sink: &S,
    bundle: &mut Bundle,
    target: BundleStatus,
    role: Role,
) -> Result<(), BundleTransitionError>
where
    S: store::BundleUpdateSink,
{
    debug!(
        bundle_id = %bundle.id,
        from = ?bundle.status,
        target = ?target,
        role = ?role,
        "transition_and_persist_bundle: entry"
    );
    let expected_updated_at = bundle.updated_at;
    let result = bundle
        .transition(target, role)
        .map_err(|e| BundleTransitionError::Fsm(format!("transition: {e}")))?;
    if result == domain::Transition::Unchanged {
        debug!(
            bundle_id = %bundle.id,
            status = ?bundle.status,
            "transition_and_persist_bundle: unchanged; skipping store write"
        );
        return Ok(());
    }

    let persisted = match sink
        .update(bundle.clone(), expected_updated_at, role, domain::TargetKind::Normal)
        .await
    {
        Ok(ts) => ts,
        Err(store::BundleUpdateError::Stale { expected, actual }) => {
            warn!(
                bundle_id = %bundle.id,
                expected,
                actual,
                "transition_and_persist_bundle: OCC stale"
            );
            return Err(BundleTransitionError::Stale { expected, actual });
        }
        Err(store::BundleUpdateError::Update(s)) => {
            error!(bundle_id = %bundle.id, error = %s, "transition_and_persist_bundle: persist failed");
            return Err(BundleTransitionError::Persist(s));
        }
    };
    bundle.updated_at = persisted;
    debug!(
        bundle_id = %bundle.id,
        status = ?bundle.status,
        updated_at = persisted,
        "transition_and_persist_bundle: persisted"
    );
    Ok(())
}

/// Mirror of `transition_and_persist_work` for `Plan` records. Consumed by
/// the Integrator spawn's `Active -> Complete` check once every sibling
/// Work is terminal.
///
/// Phase 6 widened the signature to take `children: Vec<Work>` per design
/// Alternatives §4 option (c-extended): the caller fetches children
/// before invoking this helper, and the sink (typically a
/// `SummaryFanout`) renders the Plan summary from `(plan, children)`.
/// Extra Plan-level counts surfaced on the `plan: terminal-state
/// summary` event. Computed at the daemon call site from the store
/// (Tick / Bundle queries) so the helper itself doesn't need a store
/// handle. `Default::default()` is acceptable for tests; production
/// callers should populate from real queries.
#[derive(Default)]
pub struct PlanSummaryExtras {
    pub ticks: u64,
    pub bundles_accepted: u64,
    pub bundles_rejected: u64,
}

/// Compute `PlanSummaryExtras` for a finishing Plan. `ticks` is a
/// direct query; `bundles_accepted` / `bundles_rejected` walk the
/// Plan's child Works fanned out to each Work's Bundle list. A
/// `bundle.status` of `Reviewed` / `Accepted` / `Integrating` /
/// `Merged` counts as accepted; `Rejected` / `IntegrationFailed`
/// counts as rejected. Other statuses (Triaged, ProposedNoop, etc.)
/// are pre-decision and don't contribute to either count.
///
/// Best-effort: any individual store error is folded into the
/// running counter rather than aborting — a missing Bundle list for
/// one Work shouldn't block the Plan's terminal summary.
pub(crate) async fn compute_plan_summary_extras(
    store: &Store,
    plan_id: &PlanId,
    children: &[Work],
) -> PlanSummaryExtras {
    let mut extras = PlanSummaryExtras::default();
    if let Ok(ticks) = store.ticks().list_by_plan_id(plan_id).await {
        extras.ticks = ticks.len() as u64;
    }
    for work in children {
        let Ok(bundles) = store.bundles().list_by_work_id(&work.id).await else {
            continue;
        };
        for b in bundles {
            match b.status {
                BundleStatus::Reviewed | BundleStatus::Accepted | BundleStatus::Integrating | BundleStatus::Merged => {
                    extras.bundles_accepted += 1
                }
                BundleStatus::Rejected | BundleStatus::IntegrationFailed => extras.bundles_rejected += 1,
                _ => {}
            }
        }
    }
    extras
}

pub async fn transition_and_persist_plan<S>(
    sink: &S,
    plan: &mut Plan,
    children: Vec<Work>,
    target: PlanStatus,
    role: Role,
    extras: PlanSummaryExtras,
    override_: bool,
) -> Result<(), String>
where
    S: store::PlanUpdateSink,
{
    // OCC snapshot BEFORE the FSM transition bumps `plan.updated_at`.
    let expected_updated_at = plan.updated_at;
    let result = if override_ {
        plan.override_status(target, role)
            .map_err(|e| format!("fsm override rejected: {e}"))?
    } else {
        plan.transition(target, role)
            .map_err(|e| format!("fsm transition rejected: {e}"))?
    };
    if result == domain::Transition::Unchanged {
        return Ok(());
    }

    // Snapshot the per-state Work counts BEFORE the sink moves
    // `children` into its `update` call.
    let (works_done, works_failed, works_blocked) = if plan.status.is_terminal() {
        let mut done = 0u64;
        let mut failed = 0u64;
        let mut blocked = 0u64;
        for w in &children {
            match w.status {
                WorkStatus::Done | WorkStatus::Integrated => done += 1,
                WorkStatus::Abandoned | WorkStatus::Superseded => failed += 1,
                WorkStatus::Blocked => blocked += 1,
                _ => {}
            }
        }
        (Some(done), Some(failed), Some(blocked))
    } else {
        (None, None, None)
    };
    let total_works = if plan.status.is_terminal() { Some(children.len() as u64) } else { None };
    let plan_terminal = plan.status.is_terminal();
    let plan_id = plan.id.clone();
    let plan_status = plan.status;

    // Phase 9: pass the FSM intent through to the store chokepoint.
    let kind = if override_ {
        domain::TargetKind::Override
    } else {
        domain::TargetKind::Normal
    };
    let persisted = sink
        .update(plan.clone(), children, expected_updated_at, role, kind)
        .await
        .map_err(|e| format!("plans().update: {e}"))?;
    plan.updated_at = persisted;

    // Phase 8: per-Plan terminal summary. `ticks`, `bundles_accepted`,
    // `bundles_rejected` come from the daemon-side `extras`
    // (computed from store queries at the call site, where a store
    // handle is available). `total_input_tokens` / `total_output_tokens`
    // / `total_cost_usd` are deferred — those numbers live on
    // `ProcessSnapshot` / `MeteredLlmClient` and would require
    // threading a snapshot handle in. Tracked as Open Question on
    // `docs/design/2026-05-09-comprehensive-telemetry.md`.
    if plan_terminal {
        info!(
            plan_id = %plan_id,
            terminal_state = ?plan_status,
            role = ?role,
            total_works = total_works.unwrap_or(0),
            works_done = works_done.unwrap_or(0),
            works_failed = works_failed.unwrap_or(0),
            works_blocked = works_blocked.unwrap_or(0),
            ticks = extras.ticks,
            bundles_accepted = extras.bundles_accepted,
            bundles_rejected = extras.bundles_rejected,
            "plan: terminal-state summary"
        );
    }
    Ok(())
}
