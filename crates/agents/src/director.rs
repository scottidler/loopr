//! Director: long-lived per-Plan Opus supervisor.
//!
//! `run_director` polls TaskStore via `DirectorStore`, assembles a state
//! summary via `context::build_for_director`, calls the LLM, parses
//! `DirectorAction`s, and dispatches them through `WorkSpawner`.
//!
//! The Director supplements the daemon's reactive dep-gate promotion (1.1)
//! and inline Reviewer to Integrator chain (Stage 8) by handling the
//! states those paths cannot resolve on their own: `Reviewed` Bundles
//! awaiting acceptance policy, `Blocked` Works needing recovery, and the
//! goal-completion audit.
//!
//! Loop shape (mirrors Implementer's outer-loop + parse-retry sub-loop):
//! 1. Reconcile sweep promotes `Integrated` Works to `Done` and reports
//!    GoalComplete (`Ok(true)` exits the run).
//! 2. Build `context::DirectorState` from store snapshots.
//! 3. Call `context.build_for_director` to assemble system prompt + state
//!    user message.
//! 4. Inner parse-retry sub-loop: re-prompt on parse failure within the
//!    same iteration; only successful turns enter cross-iteration history,
//!    avoiding the user/user adjacency that would arise if a failed turn's
//!    error message survived into the next iteration's `build_for_director`
//!    call (which always appends a fresh state user message).
//! 5. Lifeguard `record_parse_failure` after the sub-loop exhausts its
//!    requery budget; escalation returns `DirectorError::Lifeguard`.
//! 6. Execute parsed actions through `WorkSpawner`; `NeedHelp` exits with
//!    `DirectorError::NeedHelp` and no restart.
//! 7. Sleep `poll_interval_secs` (or `idle_interval_secs` when no actions
//!    were taken), interruptible by `deps.shutdown`.
//!
//! Restart story: `max_restarts` retries on transient failures (Llm,
//! Store, Context, Parse, Lifeguard). History is cleared on every restart
//! because the LLM context that led to the error is suspect; the reconcile
//! sweep re-derives ground truth from the store at the top of every
//! iteration.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{debug, info, instrument, warn};

use context::{ContextBuilder, ContextError, DirectorState as CtxDirectorState};
use domain::{
    Bundle, BundleId, BundleStatus, NoteId, OperatorNote, Plan, PlanId, PlanStatus, Role, Work, WorkId, WorkStatus,
    now_millis,
};
use llm::{LlmClient, Message};
use store::StoreError;

use crate::config::DirectorConfig;
use crate::lifeguard::{Decision, Lifeguard};

/// LLM-emitted instruction. Serialized as `{"action": "<kind>", ...}`.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DirectorAction {
    /// Accept a Reviewed Bundle; spawn Integrator.
    AcceptBundle { bundle_id: String },
    /// FSM override on a Work. Primary Phase 1 recovery path.
    OverrideWork {
        work_id: String,
        target_status: String,
        reason: String,
    },
    /// Explicit Work assignment. Edge case only: the dep-gate reactive path
    /// (1.1) handles the common case. Director emits this when it observes
    /// a Ready Work that the reactive path missed (e.g. a dep resolved
    /// after a daemon restart before the promotion sweep ran).
    /// `WorkSpawner::assign_work` validates dep-gate before spawning.
    AssignWork { work_id: String },
    /// No actions needed this iteration. NOT a FSM transition; Director
    /// stays in Executing and resumes polling after `idle_interval_secs`.
    Done { summary: String },
    /// Unrecoverable state; exit immediately. NOT a FSM transition; this
    /// exits the Director task with `DirectorError::NeedHelp` and no
    /// restart.
    NeedHelp { reason: String },
}

/// Errors emitted by `run_director`.
#[derive(Debug, Error)]
pub enum DirectorError {
    #[error("llm error: {0}")]
    Llm(#[from] llm::LlmError),
    #[error("lifeguard escalation: {0}")]
    Lifeguard(String),
    #[error("director emitted need_help: {0}")]
    NeedHelp(String),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("context error: {0}")]
    Context(#[from] ContextError),
    #[error("invalid id: {0}")]
    Id(String),
    #[error("fsm transition failed: {0}")]
    Fsm(String),
}

/// Fire-and-forget spawn surface injected into `run_director`. Implemented
/// by `Arc<DaemonContext<L>>` in `crates/loopr` (Phase 3). Tests inject a
/// fake.
pub trait WorkSpawner: Send + Sync + 'static {
    /// Transition Bundle Reviewed to Accepted; spawn Integrator task.
    fn accept_bundle(&self, bundle_id: BundleId);
    /// FSM override on a Work. The impl validates the transition is
    /// permitted before firing.
    fn override_work(&self, work_id: WorkId, target_status: WorkStatus, reason: String);
    /// Dep-gate check + spawn Implementer for a Ready Work. No-op if the
    /// Work's deps are not all Done.
    fn assign_work(&self, work_id: WorkId);

    // ---------- Phase 2 stuck-state recovery surface ----------
    //
    // These five methods support `reconcile_director`'s detection of
    // crash-interrupted FSM states (Triaged-no-Reviewer, Accepted-no-
    // Integrator, InProgress-no-Implementer). The `list_running_*_ids`
    // helpers expose the daemon's per-task sidecar maps so the sweep
    // can decide whether a stuck record needs re-firing. The
    // `spawn_reviewer` / `spawn_integrator` methods are the re-fire
    // path itself.
    //
    // Defaults are intentionally narrow so existing test fakes do not
    // break: list helpers default to empty `Vec` (no live tasks),
    // spawn helpers default to no-op. Production impls in `crates/loopr`
    // override all five.

    /// Re-spawn a Reviewer task for a Bundle stuck in `Triaged` status.
    /// Production impl validates Bundle is currently Triaged; logs and
    /// skips otherwise (re-running a Reviewer on already-Reviewed redoes
    /// work). Default no-op.
    fn spawn_reviewer(&self, _bundle_id: BundleId) {}

    /// Re-spawn an Integrator task for a Bundle stuck in `Accepted`
    /// status with no live Integrator. Production impl validates Bundle
    /// is currently Accepted; logs and skips otherwise. Default no-op.
    fn spawn_integrator(&self, _bundle_id: BundleId) {}

    /// Recover an `InProgress` Work whose Implementer is no longer live.
    /// Reactive recovery, not Director judgment: the reconcile sweep
    /// detected a stuck FSM state and is reverting it so the dep-gate
    /// watcher can re-promote `Ready -> InProgress` and re-spawn the
    /// Implementer. Production impl applies the `InProgress -> Ready`
    /// override under `Role::Reactor` (the override table is
    /// Reactor-only by design) and bumps `attempt_count` via the
    /// Layer-1 increment site in `transition_and_persist_work`. Default
    /// no-op.
    fn recover_in_progress_work(&self, _work_id: WorkId, _reason: String) {}

    /// Snapshot of Work IDs currently being worked on by a live
    /// Implementer task. Used by reconcile to detect InProgress Works
    /// whose Implementer panicked. Default empty (test fakes can
    /// override if behavior matters to the test).
    fn list_running_work_ids(&self) -> Vec<WorkId> {
        Vec::new()
    }

    /// Snapshot of Bundle IDs currently being reviewed by a live
    /// Reviewer task. Default empty.
    fn list_running_reviewer_bundle_ids(&self) -> Vec<BundleId> {
        Vec::new()
    }

    /// Snapshot of Bundle IDs currently being integrated by a live
    /// Integrator task. Default empty.
    fn list_running_integrator_bundle_ids(&self) -> Vec<BundleId> {
        Vec::new()
    }
}

/// Narrow read+write store surface the Director needs. Five methods:
/// the original two list helpers, plus `get_work` / `get_plan` /
/// `update_plan` consumed by the retry-budget enforcement path
/// (`max_work_attempts` -> Plan::Stalled). Kept narrow on purpose so
/// test fakes stay small.
#[trait_variant::make(Send)]
pub trait DirectorStore: Send + Sync + 'static {
    async fn list_works_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Work>, StoreError>;
    async fn list_bundles_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Bundle>, StoreError>;
    async fn get_work(&self, work_id: &WorkId) -> Result<Work, StoreError>;
    async fn get_plan(&self, plan_id: &PlanId) -> Result<Plan, StoreError>;
    async fn update_plan(&self, plan: Plan) -> Result<(), StoreError>;
    /// Phase 9: list the unread operator notes for a Plan, oldest-first.
    /// `run_director_inner` calls this at the top of every iteration;
    /// non-empty returns synthesize a `PatternObservation::OperatorNoteArrived`
    /// for the mode FSM and render into the user prompt's `## Operator Notes`
    /// section.
    async fn list_unread_notes_for_plan(&self, plan_id: &PlanId) -> Result<Vec<OperatorNote>, StoreError>;
    /// Phase 9: mark a batch of notes read after a successful LLM round-trip.
    /// Render-then-mark ordering is load-bearing: marking before render
    /// means a daemon crash between mark and render loses the note;
    /// marking after a successful LLM call ensures the note has been
    /// observed before its `read_at` is stamped.
    async fn mark_notes_read(&self, note_ids: &[NoteId]) -> Result<(), StoreError>;
}

/// `DirectorStore` impl on `store::Store`. `list_bundles_for_plan` walks
/// the Plan's Works and unions their `list_by_work_id` queries — per the
/// design doc, the in-memory join is acceptable for first-gate Plan sizes
/// (single-digit Works per Plan, low-double-digit Bundles per Work).
impl DirectorStore for store::Store {
    async fn list_works_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Work>, StoreError> {
        self.works().list_by_parent_id(plan_id).await
    }

    async fn list_bundles_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Bundle>, StoreError> {
        let works = self.works().list_by_parent_id(plan_id).await?;
        let mut bundles = Vec::new();
        for work in &works {
            let chunk = self.bundles().list_by_work_id(&work.id).await?;
            bundles.extend(chunk);
        }
        Ok(bundles)
    }

    async fn get_work(&self, work_id: &WorkId) -> Result<Work, StoreError> {
        self.works().get(work_id).await
    }

    async fn get_plan(&self, plan_id: &PlanId) -> Result<Plan, StoreError> {
        self.plans().get(plan_id).await
    }

    async fn update_plan(&self, plan: Plan) -> Result<(), StoreError> {
        self.plans().update(plan).await
    }

    async fn list_unread_notes_for_plan(&self, plan_id: &PlanId) -> Result<Vec<OperatorNote>, StoreError> {
        self.notes().list_unread_for_plan(plan_id).await
    }

    async fn mark_notes_read(&self, note_ids: &[NoteId]) -> Result<(), StoreError> {
        self.notes().mark_read(note_ids, now_millis()).await
    }
}

/// Forwarding `DirectorStore` impl for `Arc<S>`. Mirrors the pattern used
/// by `BundleSink` in this crate. (No `&S` forwarding impl: `DirectorStore`
/// requires `'static` ownership for `tokio::spawn` of the per-Plan task,
/// and a borrowed reference cannot satisfy that bound.)
impl<S: DirectorStore + ?Sized> DirectorStore for Arc<S> {
    async fn list_works_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Work>, StoreError> {
        (**self).list_works_for_plan(plan_id).await
    }

    async fn list_bundles_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Bundle>, StoreError> {
        (**self).list_bundles_for_plan(plan_id).await
    }

    async fn get_work(&self, work_id: &WorkId) -> Result<Work, StoreError> {
        (**self).get_work(work_id).await
    }

    async fn get_plan(&self, plan_id: &PlanId) -> Result<Plan, StoreError> {
        (**self).get_plan(plan_id).await
    }

    async fn update_plan(&self, plan: Plan) -> Result<(), StoreError> {
        (**self).update_plan(plan).await
    }

    async fn list_unread_notes_for_plan(&self, plan_id: &PlanId) -> Result<Vec<OperatorNote>, StoreError> {
        (**self).list_unread_notes_for_plan(plan_id).await
    }

    async fn mark_notes_read(&self, note_ids: &[NoteId]) -> Result<(), StoreError> {
        (**self).mark_notes_read(note_ids).await
    }
}

/// Dependencies injected into `run_director`. Mirrors the Implementer's
/// `Deps<L, T, S, C>` pattern: one generic flows through the function
/// signature; concrete trait bounds live here.
pub struct DirectorDeps<L, S, C, P>
where
    L: LlmClient,
    S: DirectorStore,
    C: ContextBuilder,
    P: WorkSpawner,
{
    pub llm: L,
    pub store: S,
    pub context: C,
    pub spawner: P,
    pub config: DirectorConfig,
    /// Fires when the daemon is shutting down; Director exits its sleep loop.
    /// Injected from `DaemonContext::shutdown_notify` at spawn time.
    pub shutdown: Arc<Notify>,
    /// Phase 9: per-Plan wake-up channel. The IPC handler for
    /// `director.chat` calls `notify_one()` on the matching Plan's
    /// `Notify` after persisting the OperatorNote so the Director
    /// preempts its inter-iteration sleep instead of waiting for the
    /// next `idle_interval_secs` tick. One `Arc<Notify>` per Plan
    /// keeps notes routed precisely; a process-wide channel would
    /// spuriously wake every Director task on every note.
    pub operator_notify: Arc<Notify>,
}

/// Parse the LLM response into a `Vec<DirectorAction>`.
pub fn parse_director_actions(response: &str) -> Result<Vec<DirectorAction>, DirectorError> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return Err(DirectorError::Parse("empty response".to_string()));
    }
    if let Ok(actions) = serde_json::from_str::<Vec<DirectorAction>>(trimmed) {
        return Ok(actions);
    }
    if let Ok(action) = serde_json::from_str::<DirectorAction>(trimmed) {
        return Ok(vec![action]);
    }
    let err = serde_json::from_str::<Vec<DirectorAction>>(trimmed)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| "unknown parse failure".to_string());
    Err(DirectorError::Parse(err))
}

/// PascalCase string for a `WorkStatus`. Mirrors the explicit-match style
/// `summary/work.rs` and `summary/plan.rs` already use; Display is
/// lowercase per strum, and Debug format is not a stable API surface.
fn work_status_str(status: WorkStatus) -> &'static str {
    use WorkStatus::*;
    match status {
        Draft => "Draft",
        Pending => "Pending",
        Ready => "Ready",
        InProgress => "InProgress",
        Blocked => "Blocked",
        InReview => "InReview",
        Integrated => "Integrated",
        Done => "Done",
        Superseded => "Superseded",
        Abandoned => "Abandoned",
    }
}

/// PascalCase string for a `BundleStatus`. Symmetric with `work_status_str`.
fn bundle_status_str(status: BundleStatus) -> &'static str {
    use BundleStatus::*;
    match status {
        Proposed => "Proposed",
        Triaged => "Triaged",
        Reviewed => "Reviewed",
        Accepted => "Accepted",
        Integrating => "Integrating",
        Merged => "Merged",
        Rejected => "Rejected",
        IntegrationFailed => "IntegrationFailed",
        Superseded => "Superseded",
    }
}

/// Parse a `DirectorAction::OverrideWork.target_status` string back into a
/// typed `WorkStatus`. Round-trips with `work_status_str`.
fn parse_work_status(s: &str) -> Result<WorkStatus, DirectorError> {
    use WorkStatus::*;
    match s {
        "Draft" => Ok(Draft),
        "Pending" => Ok(Pending),
        "Ready" => Ok(Ready),
        "InProgress" => Ok(InProgress),
        "Blocked" => Ok(Blocked),
        "InReview" => Ok(InReview),
        "Integrated" => Ok(Integrated),
        "Done" => Ok(Done),
        "Superseded" => Ok(Superseded),
        "Abandoned" => Ok(Abandoned),
        other => Err(DirectorError::Parse(format!("unknown WorkStatus: {other}"))),
    }
}

/// Build the display-oriented `context::DirectorState` from store snapshots.
/// All `WorkStatus` and `BundleStatus` values are stringified at this seam
/// so `context` does not import `domain` FSM enums.
pub async fn build_director_state<S: DirectorStore>(
    plan_id: &PlanId,
    store: &S,
) -> Result<CtxDirectorState, DirectorError> {
    let works = store.list_works_for_plan(plan_id).await?;
    let bundles = store.list_bundles_for_plan(plan_id).await?;

    let work_lines = works
        .iter()
        .map(|w| context::WorkLine {
            id: w.id.to_string(),
            title: w.title.clone(),
            status: work_status_str(w.status).to_string(),
            attempt_count: w.attempt_count,
        })
        .collect();
    let bundle_lines = bundles
        .iter()
        .map(|b| context::BundleLine {
            id: b.id.to_string(),
            work_id: b.work_id.to_string(),
            status: bundle_status_str(b.status).to_string(),
        })
        .collect();

    Ok(CtxDirectorState {
        plan_id: plan_id.to_string(),
        // Phase 6: `run_director_inner` overrides `mode` after this
        // call with the current tracker mode. Empty here means the
        // builder renders "Normal" — the default for fresh state
        // (build_director_state used outside the run loop is a
        // test-only path that doesn't exercise modes).
        mode: String::new(),
        works: work_lines,
        bundles: bundle_lines,
        blocked_reason: None,
        // Phase 9: `run_director_inner` overrides `operator_notes`
        // before calling the context builder. Empty here is the
        // test-only path's no-notes baseline.
        operator_notes: Vec::new(),
    })
}

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

/// Long-running per-Plan supervisor task.
#[instrument(
    name = "director.run",
    level = "info",
    skip_all,
    fields(
        plan_id = %plan_id,
        iteration = tracing::field::Empty,
        restart = tracing::field::Empty,
        restart_reason = tracing::field::Empty,
    ),
    err,
)]
pub async fn run_director<L, S, C, P>(plan_id: &PlanId, deps: &DirectorDeps<L, S, C, P>) -> Result<(), DirectorError>
where
    L: LlmClient,
    S: DirectorStore,
    C: ContextBuilder,
    P: WorkSpawner,
{
    let max_restarts = deps.config.max_restarts;
    let mut restart: u32 = 0;

    'restart: loop {
        let result: Result<(), DirectorError> = run_director_inner(plan_id, deps).await;
        match result {
            Ok(()) => return Ok(()),
            Err(DirectorError::NeedHelp(reason)) => return Err(DirectorError::NeedHelp(reason)),
            Err(e) if restart < max_restarts => {
                restart += 1;
                let reason = restart_reason_for(&e);
                tracing::Span::current().record("restart", restart);
                tracing::Span::current().record("restart_reason", reason);
                warn!(error = %e, restart, restart_reason = reason, max_restarts, "director restart");
                continue 'restart;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Phase 7: classify a `DirectorError` into a stable `&'static str`
/// reason so log readers can grep `restart_reason="llm_retryable"`
/// vs `restart_reason="parse_failure"` without parsing the error
/// message. The set is closed; new error variants must extend this
/// table on landing.
fn restart_reason_for(err: &DirectorError) -> &'static str {
    match err {
        DirectorError::Llm(_) => "llm_retryable",
        DirectorError::Parse(_) => "parse_failure",
        DirectorError::Context(_) => "context_failure",
        DirectorError::Store(_) => "store_failure",
        DirectorError::Id(_) => "id_failure",
        DirectorError::Lifeguard(_) => "lifeguard_escalation",
        DirectorError::NeedHelp(_) => "need_help",
        DirectorError::Fsm(_) => "fsm_failure",
    }
}

/// Phase 7: routing event emitted when the Director's LLM-emitted
/// `accept_bundle` action fires. The Director does not transition
/// the Bundle itself (the daemon's WorkSpawner does); this span
/// records that the Director chose to accept this Bundle so a
/// `director.accept_bundle` grep finds every per-Bundle decision
/// without parsing the iteration log.
#[instrument(
    name = "director.accept_bundle",
    level = "info",
    skip_all,
    fields(plan_id = %plan_id, bundle_id = %bundle_id),
)]
pub fn director_accept_bundle<P: WorkSpawner>(plan_id: &PlanId, bundle_id: &BundleId, spawner: &P) {
    spawner.accept_bundle(bundle_id.clone());
    info!(
        plan_id = %plan_id,
        bundle_id = %bundle_id,
        "director: accept_bundle dispatched"
    );
}

/// One restart-attempt of the Director loop. Returns `Ok(())` on
/// GoalComplete; surface-level errors propagate to the outer restart
/// dispatcher.
async fn run_director_inner<L, S, C, P>(plan_id: &PlanId, deps: &DirectorDeps<L, S, C, P>) -> Result<(), DirectorError>
where
    L: LlmClient,
    S: DirectorStore,
    C: ContextBuilder,
    P: WorkSpawner,
{
    let mut history: Vec<Message> = Vec::new();
    // `max_repeat_action` is irrelevant for Director; we never call
    // `lifeguard.check_action`. Pass 0 so the field is initialised; only
    // `record_parse_failure` is exercised, governed by `max_parse_failures`.
    let mut lifeguard = Lifeguard::new(0, deps.config.max_parse_failures);
    let mut iteration: u32 = 0;

    let grace_ms: i64 = (deps.config.reconcile_grace_secs as i64).saturating_mul(1000);

    // Phase 4-6: in-memory pattern tracker + mode FSM. Both reset on
    // Director restart (the outer `run_director` loop allocates a fresh
    // `run_director_inner` invocation). Mode is task-local; the user
    // prompt's mode label is the only signal the LLM sees about which
    // `## Mode-Aware Recovery` subsection to apply.
    let mut pattern_tracker = DirectorPatternTracker::new(deps.config.patterns.clone());
    let mut current_mode = DirectorMode::Normal;

    loop {
        // 1. Reconcile sweep.
        let goal_done = reconcile_director(plan_id, &deps.store, &deps.spawner, grace_ms).await?;
        if goal_done {
            info!(plan_id = %plan_id, "director: goal complete; exiting");
            return Ok(());
        }

        iteration += 1;
        tracing::Span::current().record("iteration", iteration);
        info!(iteration, plan_id = %plan_id, "director iteration start");

        // 2. Build state; stamp the current mode so the user-prompt
        //    label tells the LLM which mode-aware block to apply.
        let mut state = build_director_state(plan_id, &deps.store).await?;
        state.mode = current_mode.as_str().to_string();

        // 2a. Phase 9: surface any unread operator notes. Notes are
        //     rendered into the `## Operator Notes` section of the user
        //     prompt; their arrival also drives the mode FSM via the
        //     out-of-band `PatternObservation::OperatorNoteArrived`
        //     variant (NOT emitted by the pattern tracker). Demotion
        //     from Conservative/NeedsOperator -> Normal also resets
        //     the tracker's `no_progress_streak` so the next iteration
        //     starts the no-progress detector fresh under operator
        //     watch. Mark-read is deferred until AFTER a successful
        //     LLM round-trip (step 4) so a parse-loop failure or LLM
        //     error does not lose the note's content.
        let unread_notes = deps.store.list_unread_notes_for_plan(plan_id).await?;
        let unread_note_ids: Vec<NoteId> = unread_notes.iter().map(|n| n.id.clone()).collect();
        if !unread_notes.is_empty() {
            state.operator_notes = unread_notes.iter().map(|n| n.message.clone()).collect();
            let next = next_mode(current_mode, &PatternObservation::OperatorNoteArrived);
            if next != current_mode {
                info!(
                    plan_id = %plan_id,
                    iteration,
                    from = current_mode.as_str(),
                    to = next.as_str(),
                    trigger = "operator_note",
                    note_count = unread_notes.len(),
                    "director.mode_change"
                );
                pattern_tracker.reset_no_progress_streak();
                current_mode = next;
                // Re-stamp the mode label so the user prompt reflects
                // the post-demotion state on this very iteration.
                state.mode = current_mode.as_str().to_string();
            } else {
                debug!(
                    plan_id = %plan_id,
                    iteration,
                    mode = current_mode.as_str(),
                    note_count = unread_notes.len(),
                    "director: operator note observed; mode unchanged (idempotent edge)"
                );
            }
        }

        // 3. Context + LLM.
        let assembled = deps
            .context
            .build_for_director(&state, &history, deps.config.token_budget)?;

        // 4. Same-iteration parse-retry sub-loop. Only a successful turn
        //    enters cross-iteration history; failed turns stay local to the
        //    sub-loop, preventing user/user adjacency on the next iteration.
        let mut messages = assembled.messages.clone();
        let actions: Vec<DirectorAction> = loop {
            let (raw, _usage) = deps
                .llm
                .complete_free(&assembled.system_prompt, &messages, Some(deps.config.model.as_str()))
                .await?;
            match parse_director_actions(&raw) {
                Ok(parsed) => {
                    lifeguard.reset_parse_failures();
                    // Append the successful turn (state user msg + assistant)
                    // to cross-iteration history before breaking.
                    if let Some(last) = assembled.messages.last() {
                        history.push(last.clone());
                    }
                    history.push(Message::assistant(raw));
                    // Phase 9: render-then-mark. The notes have now
                    // been observed by the LLM (their bodies were in
                    // the rendered user prompt), so it is safe to
                    // stamp `read_at`. Failures here log and continue
                    // — a re-read on the next iteration is benign
                    // (the body just re-renders), so we never let
                    // taskstore hiccups drop the iteration.
                    if !unread_note_ids.is_empty()
                        && let Err(e) = deps.store.mark_notes_read(&unread_note_ids).await
                    {
                        warn!(
                            plan_id = %plan_id,
                            iteration,
                            note_count = unread_note_ids.len(),
                            error = %e,
                            "director: mark_notes_read failed; notes will re-render next iteration"
                        );
                    }
                    break parsed;
                }
                Err(e) => {
                    warn!(iteration, error = %e, "director parse failure in sub-loop");
                    messages.push(Message::assistant(raw));
                    messages.push(Message::user(format!(
                        "ERROR: Could not parse response as a JSON array of action objects. {e}\n\
                         Respond with ONLY a valid JSON array of action objects."
                    )));
                    let requeries_used = (messages.len() - 1) / 2;
                    if requeries_used as u32 >= deps.config.max_requeries {
                        if let Decision::Escalate(reason) = lifeguard.record_parse_failure() {
                            return Err(DirectorError::Lifeguard(reason));
                        }
                        // Lifeguard counts the strike but allows the outer
                        // loop to try again on a fresh iteration.
                        break Vec::new();
                    }
                }
            }
        };

        // 5. Execute parsed actions.
        let mut took_action = false;
        for action in &actions {
            match action {
                DirectorAction::AcceptBundle { bundle_id } => {
                    let id = bundle_id
                        .parse::<BundleId>()
                        .map_err(|e| DirectorError::Id(format!("bundle_id={bundle_id}: {e}")))?;
                    director_accept_bundle(plan_id, &id, &deps.spawner);
                    took_action = true;
                }
                DirectorAction::OverrideWork {
                    work_id,
                    target_status,
                    reason,
                } => {
                    let wid = work_id
                        .parse::<WorkId>()
                        .map_err(|e| DirectorError::Id(format!("work_id={work_id}: {e}")))?;
                    let target = parse_work_status(target_status)?;
                    // Layer 2 retry-budget cap. Only fires when the
                    // Director is asking to push a Work back to Ready
                    // (the recovery path); other override targets are
                    // unrelated to retry semantics. Persist Plan ->
                    // Stalled BEFORE returning NeedHelp so a daemon
                    // restart's `startup_reconcile_directors` skips the
                    // Plan instead of respawning a Director that would
                    // immediately re-exhaust the same budget. Layer 1
                    // (in `transition_and_persist_work`) makes
                    // `attempt_count` 1-based; a Work that has reached
                    // Ready three times has `attempt_count = 3`, and
                    // with default `max_work_attempts = 3` the cap
                    // refuses the 4th retry.
                    if target == WorkStatus::Ready {
                        let work = deps.store.get_work(&wid).await?;
                        if work.attempt_count >= deps.config.max_work_attempts {
                            warn!(
                                plan_id = %plan_id,
                                work_id = %wid,
                                attempt_count = work.attempt_count,
                                max_work_attempts = deps.config.max_work_attempts,
                                "director: retry budget exhausted; transitioning Plan -> Stalled"
                            );
                            let mut plan = deps.store.get_plan(plan_id).await?;
                            plan.transition(PlanStatus::Stalled, Role::Director)
                                .map_err(|e| DirectorError::Fsm(format!("plan -> Stalled rejected: {e}")))?;
                            deps.store.update_plan(plan).await?;
                            return Err(DirectorError::NeedHelp(format!(
                                "retry budget exhausted on work {wid} (attempt_count={} >= max_work_attempts={})",
                                work.attempt_count, deps.config.max_work_attempts
                            )));
                        }
                    }
                    deps.spawner.override_work(wid, target, reason.clone());
                    took_action = true;
                }
                DirectorAction::AssignWork { work_id } => {
                    let wid = work_id
                        .parse::<WorkId>()
                        .map_err(|e| DirectorError::Id(format!("work_id={work_id}: {e}")))?;
                    deps.spawner.assign_work(wid);
                    took_action = true;
                }
                DirectorAction::Done { summary } => {
                    debug!(iteration, summary = %summary, "director done iteration");
                }
                DirectorAction::NeedHelp { reason } => {
                    return Err(DirectorError::NeedHelp(reason.clone()));
                }
            }
        }

        // 6. Pattern tracker + mode FSM. Observe ONE fingerprint per
        //    iteration (the first emitted action, or `done` when the
        //    parse-retry sub-loop exhausted with no actions). The
        //    state hash is computed from a fresh works+bundles snapshot
        //    AFTER actions executed so spawner side effects (which are
        //    fire-and-forget) are observed on the NEXT iteration —
        //    correct for cycle detection because consecutive iterations
        //    with the same hash mean "actions had no measurable effect."
        let fingerprint = actions
            .first()
            .map(fingerprint_for_action)
            .unwrap_or_else(ActionFingerprint::done);
        let works_after = deps.store.list_works_for_plan(plan_id).await?;
        let bundles_after = deps.store.list_bundles_for_plan(plan_id).await?;
        let state_hash = compute_state_hash(&works_after, &bundles_after);
        if let Some(observation) = pattern_tracker.observe(fingerprint, state_hash) {
            let next = next_mode(current_mode, &observation);
            if next != current_mode {
                let trigger = pattern_observation_trigger(&observation);
                info!(
                    plan_id = %plan_id,
                    iteration,
                    from = current_mode.as_str(),
                    to = next.as_str(),
                    trigger = trigger,
                    "director.mode_change"
                );
                current_mode = next;
            }
        }

        // 7. Sleep.
        let secs = if took_action {
            deps.config.poll_interval_secs
        } else {
            deps.config.idle_interval_secs
        };
        debug!(iteration, sleep_secs = secs, took_action, "director sleeping");
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
            _ = deps.shutdown.notified() => {
                info!(plan_id = %plan_id, "director shutdown notified; exiting");
                return Ok(());
            }
            _ = deps.operator_notify.notified() => {
                debug!(plan_id = %plan_id, iteration, "director operator-note wakeup");
            }
        }
    }
}

/// Map a parsed `DirectorAction` into the pattern tracker's
/// `ActionFingerprint`. The tracker watches for repeated mutating
/// actions and changing FSM state; this seam is where the LLM's
/// emitted action becomes a tracker-observable.
fn fingerprint_for_action(action: &DirectorAction) -> ActionFingerprint {
    match action {
        DirectorAction::AcceptBundle { bundle_id } => ActionFingerprint::accept_bundle(bundle_id),
        DirectorAction::OverrideWork {
            work_id, target_status, ..
        } => ActionFingerprint::override_work(work_id, target_status),
        DirectorAction::AssignWork { work_id } => ActionFingerprint::assign_work(work_id),
        DirectorAction::Done { .. } => ActionFingerprint::done(),
        DirectorAction::NeedHelp { .. } => ActionFingerprint::need_help(),
    }
}

/// Stable `&'static str` trigger string for the `director.mode_change`
/// event so log readers can grep `trigger="no_progress"` without
/// parsing the full observation payload.
fn pattern_observation_trigger(obs: &PatternObservation) -> &'static str {
    match obs {
        PatternObservation::SameActionTripped { .. } => "same_action",
        PatternObservation::NoProgressTripped { .. } => "no_progress",
        PatternObservation::EscalationTripped { .. } => "escalation",
        PatternObservation::Recovered => "recovered",
        PatternObservation::OperatorNoteArrived => "operator_note",
    }
}

pub mod mode;
pub mod pattern;
pub use mode::{DirectorMode, next_mode};
pub use pattern::{ActionFingerprint, DirectorPatternTracker, PatternConfig, PatternObservation, compute_state_hash};

#[cfg(test)]
mod tests;
