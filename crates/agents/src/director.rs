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

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
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
    /// Persist a Plan change under OCC. `expected_updated_at` is the
    /// snapshot the caller took BEFORE transitioning (Plans have three
    /// concurrent writers; a mismatch returns `StoreError::Stale`).
    async fn update_plan(&self, plan: Plan, expected_updated_at: i64) -> Result<(), StoreError>;
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

    async fn update_plan(&self, plan: Plan, expected_updated_at: i64) -> Result<(), StoreError> {
        self.plans().update(plan, expected_updated_at).await.map(|_| ())
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

    async fn update_plan(&self, plan: Plan, expected_updated_at: i64) -> Result<(), StoreError> {
        (**self).update_plan(plan, expected_updated_at).await
    }

    async fn list_unread_notes_for_plan(&self, plan_id: &PlanId) -> Result<Vec<OperatorNote>, StoreError> {
        (**self).list_unread_notes_for_plan(plan_id).await
    }

    async fn mark_notes_read(&self, note_ids: &[NoteId]) -> Result<(), StoreError> {
        (**self).mark_notes_read(note_ids).await
    }
}

/// Snapshot of a live Director task's mode FSM and pattern tracker
/// state, taken at the end of each iteration after the pattern tracker
/// block. Written into the daemon's per-Plan sidecar
/// (`DaemonContext::director_statuses`) and read by the `director.status`
/// IPC verb to surface "is this Plan in Conservative? what is the
/// no-progress streak?" without grepping `events.log`.
///
/// Entries are inserted on first write and removed by the Director
/// task body on exit (terminal Plan transition or daemon shutdown).
/// Absence from the map means the Plan has no live Director — the
/// status verb renders that as "director: not running".
#[derive(Debug, Clone, Serialize)]
pub struct DirectorStatusSnapshot {
    /// Current escalation mode. PascalCase wire form via `DirectorMode`'s
    /// `serde(rename_all = "PascalCase")`.
    pub mode: DirectorMode,
    /// Pattern tracker's no-progress streak depth. Compared against
    /// `PatternConfig::escalation_threshold` to predict when the mode
    /// FSM will next fire `EscalationTripped`.
    pub no_progress_streak: u32,
    /// Length of the trailing run of identical action fingerprints in
    /// the tracker's window. Compared against
    /// `PatternConfig::same_action_threshold` for the SameAction trip
    /// preview.
    pub same_action_streak: u32,
    /// Director iteration counter (1-based after the first reconcile).
    pub iteration: u32,
    /// Kind of the first action emitted this iteration:
    /// `accept_bundle` / `override_work` / `assign_work` / `done` /
    /// `need_help`. `None` when the parse-retry sub-loop exhausted
    /// with no actions.
    pub last_action_kind: Option<String>,
    /// Bundle id / Work id targeted by the first action this
    /// iteration. `None` for `done` / `need_help`.
    pub last_action_target_id: Option<String>,
    /// Millis-since-epoch timestamp captured at the end of the
    /// iteration that emitted `last_action_kind`. `None` when
    /// `last_action_kind` is `None`.
    pub last_action_ts: Option<i64>,
    /// Number of unread operator notes returned by
    /// `list_unread_notes_for_plan` at the top of this iteration.
    pub unread_note_count: usize,
    /// Phase 10 grace counter: consecutive `NeedsOperator` iterations
    /// without an operator note. Compared against
    /// `DirectorConfig::needs_operator_grace_iters` for the Stalled
    /// escalation preview.
    pub needs_operator_iters: u32,
}

/// Typed alias for the daemon's per-Plan Director status sidecar.
/// `std::sync::RwLock` so the sync IPC read path does not need to
/// `.await` to peek; writes are a single `HashMap::insert` held for
/// microseconds.
pub type DirectorStatusMap = Arc<StdRwLock<HashMap<PlanId, DirectorStatusSnapshot>>>;

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
    /// Per-Plan status sidecar: the daemon's
    /// `Arc<RwLock<HashMap<PlanId, DirectorStatusSnapshot>>>`. The
    /// Director writes the freshly built snapshot at the end of every
    /// iteration after the pattern tracker block; the IPC handler for
    /// `director.status` reads it.
    pub director_statuses: DirectorStatusMap,
}

/// Parse the LLM response into a `Vec<DirectorAction>`.
#[instrument(level = "debug", skip_all, fields(response_len = response.len()), err)]
pub fn parse_director_actions(response: &str) -> Result<Vec<DirectorAction>, DirectorError> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return Err(DirectorError::Parse("empty response".to_string()));
    }
    // Capture the FIRST (array-shape) parse error instead of re-parsing a
    // third time to recover it (bullet 16): the array shape is the
    // canonical contract, so its error is the most informative to feed
    // back to the LLM. The single-object fallback is a tolerance, not the
    // primary shape.
    let array_err = match serde_json::from_str::<Vec<DirectorAction>>(trimmed) {
        Ok(actions) => return Ok(actions),
        Err(e) => e,
    };
    if let Ok(action) = serde_json::from_str::<DirectorAction>(trimmed) {
        return Ok(vec![action]);
    }
    Err(DirectorError::Parse(array_err.to_string()))
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
#[instrument(
    level = "debug",
    skip_all,
    fields(plan_id = %plan_id, work_count = tracing::field::Empty, bundle_count = tracing::field::Empty),
    err,
)]
pub async fn build_director_state<S: DirectorStore>(
    plan_id: &PlanId,
    store: &S,
) -> Result<CtxDirectorState, DirectorError> {
    let works = store.list_works_for_plan(plan_id).await?;
    let bundles = store.list_bundles_for_plan(plan_id).await?;
    let span = tracing::Span::current();
    span.record("work_count", works.len());
    span.record("bundle_count", bundles.len());

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
        // `run_once` overrides this from `deps.config.max_work_attempts`
        // before the context builder runs (mirrors `mode`). The 3 here is
        // the test-only baseline matching the config default.
        max_work_attempts: 3,
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

/// Hard cap on the Director's in-memory `history` Vec. The prompt
/// assembler re-trims by token budget every iteration; this only bounds
/// the retained Vec so a multi-day Plan's history can't grow without
/// limit. 40 messages ≈ 20 turns — well beyond any token-budget trim.
const DIRECTOR_HISTORY_MAX_MESSAGES: usize = 40;

/// Maximum operator notes rendered into a single Director user prompt.
/// The Phase-2 design promised a "top 8 + N more" cap that was never
/// implemented, so a Plan accumulating notes could blow the prompt
/// budget. The newest 8 are rendered; older unread notes are summarized
/// by a trailing marker line.
const OPERATOR_NOTES_RENDER_CAP: usize = 8;

/// Iterations a Director session must complete before a subsequent
/// transient failure resets the restart budget. Without this, the
/// restart counter only ever climbs, so a multi-day Plan dies on its
/// `max_restarts + 1`-th transient blip EVER (bullet 7). A session that
/// ran healthily for this many iterations has proven the LLM/store path
/// works; a later blip is a fresh transient and gets a fresh budget.
const HEALTHY_ITERS_BEFORE_RESTART_RESET: u32 = 10;

/// Base unit for the exponential restart backoff. `restart` 1 sleeps
/// 1x, 2 sleeps 2x, 3 sleeps 4x, capped at `RESTART_BACKOFF_CAP`. A
/// store/LLM outage used to burn all restarts in milliseconds; the
/// backoff spaces them so a transient outage can clear.
const RESTART_BACKOFF_BASE: Duration = Duration::from_millis(250);
/// Ceiling on a single restart backoff sleep.
const RESTART_BACKOFF_CAP: Duration = Duration::from_secs(10);

/// Exponential backoff for restart attempt `restart` (1-based), capped.
fn restart_backoff(restart: u32) -> Duration {
    let shift = restart.saturating_sub(1).min(16);
    let scaled = RESTART_BACKOFF_BASE.saturating_mul(1u32 << shift);
    scaled.min(RESTART_BACKOFF_CAP)
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
        let mut iterations_completed: u32 = 0;
        let result: Result<(), DirectorError> = run_director_inner(plan_id, deps, &mut iterations_completed).await;
        match result {
            Ok(()) => return Ok(()),
            Err(DirectorError::NeedHelp(reason)) => return Err(DirectorError::NeedHelp(reason)),
            Err(e) if restart < max_restarts => {
                // Bullet 7: reset the restart budget when the failed
                // session ran healthily for a while — a long-lived Plan
                // must not accumulate restarts across unrelated transient
                // blips and die on the Nth-ever one.
                if iterations_completed >= HEALTHY_ITERS_BEFORE_RESTART_RESET {
                    debug!(plan_id = %plan_id, iterations_completed, "director: healthy run; resetting restart budget");
                    restart = 0;
                }
                restart += 1;
                let reason = restart_reason_for(&e);
                let backoff = restart_backoff(restart);
                tracing::Span::current().record("restart", restart);
                tracing::Span::current().record("restart_reason", reason);
                warn!(error = %e, restart, restart_reason = reason, max_restarts, backoff_ms = backoff.as_millis() as u64, "director restart");
                // Bullet 7: exponential backoff before the restart, so a
                // store/LLM outage doesn't burn the whole budget in
                // milliseconds. Interruptible by shutdown.
                tokio::select! {
                    biased;
                    _ = deps.shutdown.notified() => {
                        info!(plan_id = %plan_id, "director shutdown during restart backoff; exiting");
                        return Ok(());
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
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

/// Persist the Plan to `Stalled` (Director role, OCC), then return the
/// `NeedHelp` error carrying `reason`. The single owner of the
/// "stall-then-NeedHelp" exit shared by every terminal Director path:
/// retry-budget exhaustion, NeedsOperator-grace timeout, the
/// iteration/wall-clock caps, and an LLM-emitted `need_help`. Persisting
/// `Stalled` BEFORE returning is load-bearing: a daemon restart's
/// `startup_reconcile_directors` skips a Stalled Plan instead of
/// respawning a Director that would immediately re-trip the same exit
/// (the invisible-Active-stall bug for `need_help`). A re-fetch +
/// `Stalled -> Stalled` no-op is benign (idempotent if already Stalled).
/// The stall is BEST-EFFORT: a budget/need_help exit must TERMINATE the
/// Director, never restart-loop on a failed write. A get/transition/persist
/// failure is logged (`warn!`) and `NeedHelp` is returned regardless, so
/// the Director task exits cleanly; the Plan then stays Active and a
/// daemon-restart reconcile re-attempts. Returning the store error
/// instead would re-enter the restart dispatcher and re-exhaust the same
/// budget immediately.
async fn stall_plan_and_need_help<L, S, C, P>(
    deps: &DirectorDeps<L, S, C, P>,
    plan_id: &PlanId,
    reason: String,
) -> DirectorError
where
    L: LlmClient,
    S: DirectorStore,
    C: ContextBuilder,
    P: WorkSpawner,
{
    match deps.store.get_plan(plan_id).await {
        Ok(mut plan) => {
            let expected_updated_at = plan.updated_at;
            match plan.transition(PlanStatus::Stalled, Role::Director) {
                Ok(_) => {
                    if let Err(e) = deps.store.update_plan(plan, expected_updated_at).await {
                        warn!(plan_id = %plan_id, error = %e, "director: stall persist failed; exiting NeedHelp with Plan still Active");
                    }
                }
                Err(e) => {
                    warn!(plan_id = %plan_id, error = %e, "director: stall FSM transition rejected; exiting NeedHelp");
                }
            }
        }
        Err(e) => {
            warn!(plan_id = %plan_id, error = %e, "director: stall get_plan failed; exiting NeedHelp");
        }
    }
    DirectorError::NeedHelp(reason)
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

/// Outcome of a single Director iteration.
///
/// - `GoalDone` — `reconcile_director` reported all works terminal with at
///   least one Done; the outer loop returns `Ok(())` to the restart
///   dispatcher, which propagates it to the daemon as "Plan complete."
/// - `Continue { took_action }` — the iteration ran to completion without
///   tripping a terminal error; the outer loop sleeps for
///   `poll_interval_secs` (if any action was dispatched) or
///   `idle_interval_secs` (no action this turn) then runs another
///   iteration.
///
/// Terminal errors (Lifeguard escalation, NeedHelp, retry-budget cap,
/// NeedsOperator grace exceeded) propagate via `Result::Err` and bypass
/// this outcome enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectorIterOutcome {
    GoalDone,
    Continue { took_action: bool },
}

/// Per-Plan Director state that persists across iterations of the run
/// loop. Owns the cross-iteration cache the previous monolithic
/// `run_director_inner` kept on the stack: iteration counter, message
/// history, lifeguard, pattern tracker, mode FSM, NeedsOperator grace
/// counter, and the (config-derived) reconcile grace window.
///
/// Production: `run_director_inner` constructs one session per restart
/// attempt and drives it via `run_once` + a sleep/shutdown `select!`.
///
/// Tests: assertions about a single iteration ("did the Director route
/// this action to the spawner?") construct a session and call
/// `run_once(deps)` directly. There is no shutdown plumbing for those
/// tests to forget — the previous repeated-CI-hang failure mode.
pub struct DirectorSession {
    plan_id: PlanId,
    history: Vec<Message>,
    lifeguard: Lifeguard,
    iteration: u32,
    pattern_tracker: DirectorPatternTracker,
    current_mode: DirectorMode,
    needs_operator_iters: u32,
    grace_ms: i64,
}

impl DirectorSession {
    /// Construct a fresh session. `config` seeds the lifeguard parse-failure
    /// budget, the pattern tracker thresholds, and the reconcile grace
    /// window; mode starts at `Normal` and the grace counter at 0, matching
    /// the previous loop-on-the-stack initial state.
    pub fn new(plan_id: PlanId, config: &DirectorConfig) -> Self {
        // `max_repeat_action` is irrelevant for Director — `check_action`
        // is never invoked. Pass 0 so the field is initialised; only
        // `record_parse_failure` is exercised, governed by `max_parse_failures`.
        let lifeguard = Lifeguard::new(0, config.max_parse_failures);
        let grace_ms: i64 = (config.reconcile_grace_secs as i64).saturating_mul(1000);
        let pattern_tracker = DirectorPatternTracker::new(config.patterns.clone());
        Self {
            plan_id,
            history: Vec::new(),
            lifeguard,
            iteration: 0,
            pattern_tracker,
            current_mode: DirectorMode::Normal,
            needs_operator_iters: 0,
            grace_ms,
        }
    }

    /// Iteration counter — observable for the outer loop's `debug!`
    /// telemetry so the `tokio::select!` arm can log which iteration the
    /// sleep/shutdown wait belongs to.
    pub fn iteration(&self) -> u32 {
        self.iteration
    }

    /// One iteration of the Director loop, without the sleep/shutdown
    /// trailing `select!`. Mutates session state (iteration counter,
    /// pattern tracker, mode, grace counter, history, lifeguard). The
    /// outer caller is responsible for either sleeping + waiting on
    /// shutdown (the production loop) or exiting (tests asserting on
    /// a single iteration's side-effects).
    pub async fn run_once<L, S, C, P>(
        &mut self,
        deps: &DirectorDeps<L, S, C, P>,
    ) -> Result<DirectorIterOutcome, DirectorError>
    where
        L: LlmClient,
        S: DirectorStore,
        C: ContextBuilder,
        P: WorkSpawner,
    {
        let plan_id = &self.plan_id;

        // 1. Reconcile sweep.
        let goal_done = reconcile_director(plan_id, &deps.store, &deps.spawner, self.grace_ms).await?;
        if goal_done {
            info!(plan_id = %plan_id, "director: goal complete; exiting");
            return Ok(DirectorIterOutcome::GoalDone);
        }

        self.iteration += 1;
        let iteration = self.iteration;
        tracing::Span::current().record("iteration", iteration);
        info!(iteration, plan_id = %plan_id, "director iteration start");

        // 2. Build state; stamp the current mode so the user-prompt
        //    label tells the LLM which mode-aware block to apply.
        let mut state = build_director_state(plan_id, &deps.store).await?;
        state.mode = self.current_mode.as_str().to_string();
        // Render the operator-tunable retry budget into the user prompt
        // (bullet 16) so the LLM's retry guidance tracks config instead of
        // a hardcoded "3" in the cache-stable system prompt.
        state.max_work_attempts = deps.config.max_work_attempts;

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
            // Cap rendered notes at OPERATOR_NOTES_RENDER_CAP (bullet 11):
            // render the newest N (the tail of the oldest-first list) plus
            // a marker for the remainder, so a Plan that accumulated many
            // notes cannot blow the prompt budget. ALL unread notes are
            // still marked read after the round-trip (unread_note_ids
            // above is the full set) — the cap is render-only.
            let total = unread_notes.len();
            if total > OPERATOR_NOTES_RENDER_CAP {
                let skipped = total - OPERATOR_NOTES_RENDER_CAP;
                let mut rendered: Vec<String> =
                    vec![format!("[{skipped} older operator note(s) omitted; showing newest {OPERATOR_NOTES_RENDER_CAP}]")];
                rendered.extend(
                    unread_notes
                        .iter()
                        .skip(skipped)
                        .map(|n| n.message.clone()),
                );
                state.operator_notes = rendered;
            } else {
                state.operator_notes = unread_notes.iter().map(|n| n.message.clone()).collect();
            }
            let next = next_mode(self.current_mode, &PatternObservation::OperatorNoteArrived);
            if next != self.current_mode {
                info!(
                    plan_id = %plan_id,
                    iteration,
                    from = self.current_mode.as_str(),
                    to = next.as_str(),
                    trigger = "operator_note",
                    note_count = unread_notes.len(),
                    "director.mode_change"
                );
                self.pattern_tracker.reset_no_progress_streak();
                self.current_mode = next;
                // Re-stamp the mode label so the user prompt reflects
                // the post-demotion state on this very iteration.
                state.mode = self.current_mode.as_str().to_string();
            } else {
                debug!(
                    plan_id = %plan_id,
                    iteration,
                    mode = self.current_mode.as_str(),
                    note_count = unread_notes.len(),
                    "director: operator note observed; mode unchanged (idempotent edge)"
                );
            }
        }

        // 3. Context + LLM.
        let assembled = deps
            .context
            .build_for_director(&state, &self.history, deps.config.token_budget)?;

        // 4. Same-iteration parse-retry sub-loop. Only a successful turn
        //    enters cross-iteration history; failed turns stay local to the
        //    sub-loop, preventing user/user adjacency on the next iteration.
        let mut messages = assembled.messages.clone();
        // Explicit per-iteration requery counter. The old
        // `(messages.len() - 1) / 2` derivation was wrong:
        // `assembled.messages` already contains the cross-iteration
        // history, so from iteration 3 on `messages.len()` was large and
        // a single parse failure tripped the budget immediately (zero
        // requeries). This counter resets every iteration (it is a local
        // binding) and counts only THIS iteration's parse-retry turns.
        let mut requeries_used: u32 = 0;
        let actions: Vec<DirectorAction> = loop {
            let (raw, _usage) = deps
                .llm
                .complete_free(&assembled.system_prompt, &messages, Some(deps.config.model.as_str()))
                .await?;
            match parse_director_actions(&raw) {
                Ok(parsed) => {
                    self.lifeguard.reset_parse_failures();
                    // Append the successful turn (state user msg + assistant)
                    // to cross-iteration history before breaking.
                    if let Some(last) = assembled.messages.last() {
                        self.history.push(last.clone());
                    }
                    self.history.push(Message::assistant(raw));
                    // Bound in-memory history (bullet 16): build_for_director
                    // trims by token budget for the PROMPT, but self.history
                    // itself grew unbounded across a long-lived Plan. Keep
                    // only the most recent DIRECTOR_HISTORY_MAX_MESSAGES;
                    // older turns are re-derived from store ground truth each
                    // iteration anyway.
                    if self.history.len() > DIRECTOR_HISTORY_MAX_MESSAGES {
                        let excess = self.history.len() - DIRECTOR_HISTORY_MAX_MESSAGES;
                        self.history.drain(0..excess);
                    }
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
                    requeries_used += 1;
                    if requeries_used >= deps.config.max_requeries {
                        if let Decision::Escalate(reason) = self.lifeguard.record_parse_failure() {
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
                    // A hallucinated / malformed id is an LLM slip, not a
                    // daemon fault: skip the action in-iteration (no restart
                    // burn — the next iteration re-derives state and
                    // re-prompts). Pre-fix this returned DirectorError::Id
                    // and burned a precious restart.
                    let id = match bundle_id.parse::<BundleId>() {
                        Ok(id) => id,
                        Err(e) => {
                            warn!(plan_id = %plan_id, iteration, bundle_id, error = %e, "director: invalid bundle_id; skipping action (no restart)");
                            continue;
                        }
                    };
                    director_accept_bundle(plan_id, &id, &deps.spawner);
                    took_action = true;
                }
                DirectorAction::OverrideWork {
                    work_id,
                    target_status,
                    reason,
                } => {
                    let wid = match work_id.parse::<WorkId>() {
                        Ok(wid) => wid,
                        Err(e) => {
                            warn!(plan_id = %plan_id, iteration, work_id, error = %e, "director: invalid work_id; skipping action (no restart)");
                            continue;
                        }
                    };
                    let target = match parse_work_status(target_status) {
                        Ok(t) => t,
                        Err(e) => {
                            warn!(plan_id = %plan_id, iteration, target_status, error = %e, "director: invalid target_status; skipping action (no restart)");
                            continue;
                        }
                    };
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
                            return Err(stall_plan_and_need_help(
                                deps,
                                plan_id,
                                format!(
                                    "retry budget exhausted on work {wid} (attempt_count={} >= max_work_attempts={})",
                                    work.attempt_count, deps.config.max_work_attempts
                                ),
                            )
                            .await);
                        }
                    }
                    deps.spawner.override_work(wid, target, reason.clone());
                    took_action = true;
                }
                DirectorAction::AssignWork { work_id } => {
                    let wid = match work_id.parse::<WorkId>() {
                        Ok(wid) => wid,
                        Err(e) => {
                            warn!(plan_id = %plan_id, iteration, work_id, error = %e, "director: invalid work_id; skipping action (no restart)");
                            continue;
                        }
                    };
                    deps.spawner.assign_work(wid);
                    took_action = true;
                }
                DirectorAction::Done { summary } => {
                    debug!(iteration, summary = %summary, "director done iteration");
                }
                DirectorAction::NeedHelp { reason } => {
                    // An LLM-emitted need_help used to exit with the Plan
                    // still Active — an invisible stall: the daemon shows
                    // "not running (plan is Active)" while Reviewed bundles
                    // rot until restart. Persist Plan -> Stalled before
                    // returning, same as the retry-budget / grace exits.
                    warn!(plan_id = %plan_id, reason = %reason, "director emitted need_help; transitioning Plan -> Stalled");
                    return Err(stall_plan_and_need_help(deps, plan_id, reason.clone()).await);
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
        let last_action_kind = actions.first().map(action_kind_str).map(str::to_string);
        let last_action_target_id = actions.first().and_then(action_target_id);
        let fingerprint = actions
            .first()
            .map(fingerprint_for_action)
            .unwrap_or_else(ActionFingerprint::done);
        let works_after = deps.store.list_works_for_plan(plan_id).await?;
        let bundles_after = deps.store.list_bundles_for_plan(plan_id).await?;
        let state_hash = compute_state_hash(&works_after, &bundles_after);
        if let Some(observation) = self.pattern_tracker.observe(fingerprint, state_hash) {
            let next = next_mode(self.current_mode, &observation);
            if next != self.current_mode {
                let trigger = pattern_observation_trigger(&observation);
                info!(
                    plan_id = %plan_id,
                    iteration,
                    from = self.current_mode.as_str(),
                    to = next.as_str(),
                    trigger = trigger,
                    "director.mode_change"
                );
                self.current_mode = next;
            }
        }

        // 6a. Phase 10: NeedsOperator -> Stalled grace counter. The note
        //     arrival check at step 2a already demoted the mode AND reset
        //     `no_progress_streak`, but it did NOT touch this counter —
        //     do it here so a single point owns the reset logic. The
        //     trip transitions the Plan to `Stalled` (Director role) and
        //     exits with `NeedHelp`; persist BEFORE NeedHelp so a daemon
        //     restart's `startup_reconcile_directors` skips the Stalled
        //     Plan instead of respawning a Director that would re-trip
        //     the same counter.
        if self.current_mode == DirectorMode::NeedsOperator && unread_note_ids.is_empty() {
            self.needs_operator_iters = self.needs_operator_iters.saturating_add(1);
            if self.needs_operator_iters >= deps.config.needs_operator_grace_iters {
                warn!(
                    plan_id = %plan_id,
                    iteration,
                    needs_operator_iters = self.needs_operator_iters,
                    grace = deps.config.needs_operator_grace_iters,
                    "director: NeedsOperator grace exceeded; transitioning Plan -> Stalled"
                );
                return Err(stall_plan_and_need_help(
                    deps,
                    plan_id,
                    format!(
                        "NeedsOperator timeout: {} iterations without operator note",
                        self.needs_operator_iters
                    ),
                )
                .await);
            }
        } else {
            self.needs_operator_iters = 0;
        }

        // 6b. Phase 2 follow-ups (Item 3): publish the per-iteration
        //     status snapshot to the daemon's sidecar so `loopr director
        //     status <plan>` can render live mode + streak data without
        //     parsing `events.log`. Written AFTER the pattern tracker
        //     observation + Phase 10 grace counter so `mode`,
        //     `no_progress_streak`, and `needs_operator_iters` reflect
        //     this iteration's terminal state. `std::sync::RwLock`
        //     write is held only for the duration of the `insert`; no
        //     `.await` between acquire and drop. Poison degrades to a
        //     skipped snapshot — equivalent to "no update this
        //     iteration," which the IPC handler tolerates.
        let last_action_ts_ms = if last_action_kind.is_some() { Some(domain::now_millis()) } else { None };
        let snapshot = DirectorStatusSnapshot {
            mode: self.current_mode,
            no_progress_streak: self.pattern_tracker.no_progress_streak(),
            same_action_streak: self.pattern_tracker.same_action_streak(),
            iteration,
            last_action_kind,
            last_action_target_id,
            last_action_ts: last_action_ts_ms,
            unread_note_count: unread_note_ids.len(),
            needs_operator_iters: self.needs_operator_iters,
        };
        if let Ok(mut map) = deps.director_statuses.write() {
            map.insert(plan_id.clone(), snapshot);
        }

        Ok(DirectorIterOutcome::Continue { took_action })
    }
}

/// One restart-attempt of the Director loop. Returns `Ok(())` on
/// GoalComplete or shutdown; surface-level errors propagate to the
/// outer restart dispatcher. Constructs a `DirectorSession` and drives
/// it via `run_once` + a sleep/shutdown `select!` per iteration.
async fn run_director_inner<L, S, C, P>(
    plan_id: &PlanId,
    deps: &DirectorDeps<L, S, C, P>,
    iterations_completed: &mut u32,
) -> Result<(), DirectorError>
where
    L: LlmClient,
    S: DirectorStore,
    C: ContextBuilder,
    P: WorkSpawner,
{
    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    let session_start = Instant::now();
    loop {
        match session.run_once(deps).await? {
            DirectorIterOutcome::GoalDone => return Ok(()),
            DirectorIterOutcome::Continue { took_action } => {
                // Record progress for the outer restart-budget reset
                // (bullet 7): this is the count of iterations that
                // completed without erroring.
                *iterations_completed = session.iteration();
                // Absolute backstops (bullet 2): a stuck Plan must not poll
                // the LLM forever. The pattern tracker + NeedsOperator grace
                // are the primary brakes; these are the hard caps. On
                // exhaustion stall the Plan and exit with NeedHelp (same
                // posture as the other terminal Director exits).
                let elapsed_secs = session_start.elapsed().as_secs();
                if session.iteration() >= deps.config.max_iterations {
                    warn!(
                        plan_id = %plan_id,
                        iteration = session.iteration(),
                        max_iterations = deps.config.max_iterations,
                        "director: max_iterations reached; transitioning Plan -> Stalled"
                    );
                    return Err(stall_plan_and_need_help(
                        deps,
                        plan_id,
                        format!("max_iterations reached ({} iterations)", session.iteration()),
                    )
                    .await);
                }
                if elapsed_secs >= deps.config.max_wall_clock_secs {
                    warn!(
                        plan_id = %plan_id,
                        iteration = session.iteration(),
                        elapsed_secs,
                        max_wall_clock_secs = deps.config.max_wall_clock_secs,
                        "director: wall-clock budget exceeded; transitioning Plan -> Stalled"
                    );
                    return Err(stall_plan_and_need_help(
                        deps,
                        plan_id,
                        format!("wall-clock budget exceeded ({elapsed_secs}s)"),
                    )
                    .await);
                }
                let secs = if took_action {
                    deps.config.poll_interval_secs
                } else {
                    deps.config.idle_interval_secs
                };
                debug!(
                    iteration = session.iteration(),
                    sleep_secs = secs,
                    took_action,
                    "director sleeping"
                );
                // `biased;` makes shutdown win deterministically when racing the
                // sleep timer. Production: a graceful shutdown should always
                // preempt a poll-interval sleep. Tests that exercise loop
                // semantics still wire shutdown via `FakeLlm::with_shutdown_after`;
                // tests asserting on a single iteration's side-effects call
                // `DirectorSession::run_once` directly and never reach this select.
                tokio::select! {
                    biased;
                    _ = deps.shutdown.notified() => {
                        info!(plan_id = %plan_id, "director shutdown notified; exiting");
                        return Ok(());
                    }
                    _ = deps.operator_notify.notified() => {
                        debug!(plan_id = %plan_id, iteration = session.iteration(), "director operator-note wakeup");
                    }
                    _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                }
            }
        }
    }
}

/// Stable `&'static str` for a `DirectorAction` variant. Mirrors the
/// fingerprint `kind` field; used by `DirectorStatusSnapshot` so the
/// `loopr director status` verb labels actions by the same vocabulary
/// log readers see.
fn action_kind_str(action: &DirectorAction) -> &'static str {
    match action {
        DirectorAction::AcceptBundle { .. } => "accept_bundle",
        DirectorAction::OverrideWork { .. } => "override_work",
        DirectorAction::AssignWork { .. } => "assign_work",
        DirectorAction::Done { .. } => "done",
        DirectorAction::NeedHelp { .. } => "need_help",
    }
}

/// Target Bundle/Work id for the mutating actions; `None` for
/// `Done` / `NeedHelp` which carry no target.
fn action_target_id(action: &DirectorAction) -> Option<String> {
    match action {
        DirectorAction::AcceptBundle { bundle_id } => Some(bundle_id.clone()),
        DirectorAction::OverrideWork { work_id, .. } => Some(work_id.clone()),
        DirectorAction::AssignWork { work_id } => Some(work_id.clone()),
        DirectorAction::Done { .. } | DirectorAction::NeedHelp { .. } => None,
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
