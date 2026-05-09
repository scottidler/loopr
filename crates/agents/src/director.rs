//! Director Phase 1 scaffolding: types, traits, and the action parser.
//!
//! `run_director` is the long-lived Opus LLM agent that runs as a per-Plan
//! task in the daemon. It supplements the daemon's reactive dep-gate
//! promotion (1.1) and inline Reviewer→Integrator chain (Stage 8) by
//! polling TaskStore, assembling a state summary, and issuing typed
//! `DirectorAction` variants for anything the pipeline left in a stuck or
//! recoverable state.
//!
//! Phase 1 lands the scaffolding only: action enum, config, deps,
//! WorkSpawner / DirectorStore traits, and the action parser. `run_director`
//! itself is a `todo!()` stub here; the loop body, reconcile sweep, and
//! lifeguard wiring land in Phase 2.

use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Notify;

use context::ContextBuilder;
use domain::{Bundle, BundleId, PlanId, Work, WorkId, WorkStatus};
use llm::LlmClient;
use store::StoreError;

use crate::config::DirectorConfig;
use crate::lifeguard;

/// LLM-emitted instruction. Serialized as `{"action": "<kind>", ...}`.
///
/// Five variants cover the Phase 1 orchestration vocabulary:
/// - `AcceptBundle` transfers Stage 8's auto-accept into the Director's
///   explicit policy.
/// - `OverrideWork` is the primary recovery path (Blocked → Ready, etc.).
/// - `AssignWork` is the edge case where the dep-gate reactive path missed
///   a Ready Work.
/// - `Done` is a no-action iteration; the loop sleeps and resumes.
/// - `NeedHelp` is the unrecoverable exit; the task ends without restart.
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

/// Per-Plan Director lifecycle state. Two variants; the v3 coordinator FSM
/// (Interviewing → Decomposing → Planning → Executing → GoalComplete) is
/// collapsed here because v5's decomposer runs before the Director starts.
/// By the time `handle_plan_create` spawns the Director, the Plan already
/// has Works in Pending/Ready state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectorFsmState {
    /// Director is actively polling and issuing actions.
    Executing,
    /// All Works are terminal and at least one is Done. `run_director`
    /// returns `Ok(())`.
    GoalComplete,
}

/// Per-Plan state snapshot fed into the Director's prompt. Built from
/// `DirectorStore::list_works_for_plan` + `list_bundles_for_plan` on every
/// iteration.
#[derive(Debug, Clone)]
pub struct DirectorState {
    pub plan_id: PlanId,
    pub works: Vec<Work>,
    pub bundles: Vec<Bundle>,
    pub fsm_state: DirectorFsmState,
}

/// Errors emitted by `run_director`. Variants mirror the Director's failure
/// modes: LLM call failure, lifeguard escalation, `NeedHelp` action, store
/// failure, and parse failure.
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
    Context(#[from] context::ContextError),
}

/// Fire-and-forget spawn surface injected into `run_director`. Implemented
/// by `Arc<DaemonContext<L>>` in `crates/loopr` (Phase 3). Tests inject a
/// fake.
pub trait WorkSpawner: Send + Sync + 'static {
    /// Transition Bundle Reviewed → Accepted; spawn Integrator task.
    fn accept_bundle(&self, bundle_id: BundleId);
    /// FSM override on a Work. The impl validates the transition is
    /// permitted before firing.
    fn override_work(&self, work_id: WorkId, target_status: WorkStatus, reason: String);
    /// Dep-gate check + spawn Implementer for a Ready Work. No-op if the
    /// Work's deps are not all Done.
    fn assign_work(&self, work_id: WorkId);
}

/// Narrow read-only store surface the Director needs. Two methods only;
/// keeps test fakes tiny.
#[trait_variant::make(Send)]
pub trait DirectorStore: Send + Sync + 'static {
    async fn list_works_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Work>, StoreError>;
    async fn list_bundles_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Bundle>, StoreError>;
}

/// Dependencies injected into `run_director`. Mirrors the Implementer's
/// `Deps<L, T, W, S, C>` pattern: one generic flows through the function
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
}

/// Parse the LLM response into a `Vec<DirectorAction>`. The response is
/// expected to be a JSON array of action objects; a single object is also
/// tolerated and wrapped into a one-element vector. Unknown action keys
/// surface as a parse error so the lifeguard can apply a parse-failure
/// strike.
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
    let err = serde_json::from_str::<Vec<DirectorAction>>(trimmed).unwrap_err();
    Err(DirectorError::Parse(err.to_string()))
}

/// Long-running per-Plan supervisor task. Polls TaskStore via
/// `DirectorStore`, assembles a state summary, calls the LLM, parses
/// `DirectorAction`s, and dispatches them through `WorkSpawner`. Exits
/// `Ok(())` when the Plan reaches GoalComplete, `Err(NeedHelp)` on an
/// LLM-emitted `NeedHelp` action, or `Err(Lifeguard)` after the lifeguard
/// trips its restart cap.
///
/// Phase 1 is a stub; Phase 2 ships the full implementation including the
/// reconcile sweep and lifeguard wiring.
pub async fn run_director<L, S, C, P>(_plan_id: &PlanId, _deps: &DirectorDeps<L, S, C, P>) -> Result<(), DirectorError>
where
    L: LlmClient,
    S: DirectorStore,
    C: ContextBuilder,
    P: WorkSpawner,
{
    // Use lifeguard to keep the import live until Phase 2 wires it in. The
    // construction is cheap and side-effect-free.
    let _lifeguard = lifeguard::Lifeguard::new(0, 0);
    todo!("Phase 2 of director-phase-1 ships run_director's loop body")
}

#[cfg(test)]
mod tests;
