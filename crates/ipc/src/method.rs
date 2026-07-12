use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

use domain::{Plan, Work};

use crate::envelope::DaemonRequest;
use crate::error::RpcError;
use crate::records::{RecordGetParams, RecordListParams};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, IntoStaticStr)]
pub enum MethodName {
    #[strum(serialize = "system.handshake")]
    SystemHandshake,
    #[strum(serialize = "system.status")]
    SystemStatus,
    #[strum(serialize = "plan.create")]
    PlanCreate,
    #[strum(serialize = "record.list")]
    RecordList,
    #[strum(serialize = "record.get")]
    RecordGet,
    /// Operator-to-Director chat. Phase 8 of
    /// `docs/design/2026-05-09-director-phase-2.md`.
    #[strum(serialize = "director.chat")]
    DirectorChat,
    /// Operator-issued FSM override on a Plan. Used today to revive a
    /// Stalled Plan via `Stalled -> Active`. Phase 10 of
    /// `docs/design/2026-05-09-director-phase-2.md`. The handler runs
    /// the override under `Role::Director` (the only role permitted to
    /// edge from `Stalled`); other operator-driven overrides ride the
    /// same verb in the future.
    #[strum(serialize = "plan.override")]
    PlanOverride,
    /// Operator-issued FSM override on a single Work. Phase 18 of
    /// `docs/design/2026-07-11-verified-swarm.md`. Two practical edges:
    /// `Blocked -> Ready` (retry a stuck Work; the daemon re-dispatches an
    /// Implementer) and `InProgress -> Blocked` (abort an in-flight Work;
    /// the daemon fires the Work's `AbortHandle`, reaping its subprocess
    /// tree, and stamps `FailureReason::OperatorAbort`). Runs under
    /// `Role::Director` — the operator/human override role, mirroring
    /// `plan.override`.
    #[strum(serialize = "work.override")]
    WorkOverride,
    /// Read the Director's per-iteration status snapshot for a Plan.
    /// Phase 2 follow-ups (Item 3) of
    /// `docs/design/2026-05-12-director-phase-2-followups.md`. Returns
    /// mode + streaks + last action + unread-note count so operators
    /// can answer "is this Plan in Conservative? what is the
    /// no-progress streak?" without grepping `events.log`.
    #[strum(serialize = "director.status")]
    DirectorStatus,
    /// Clear the daemon's one-shot per-run budget soft-pause
    /// (`DaemonContext::budget_event_sent`) so a budget-tripped daemon
    /// resumes spawning implementers after the operator raises
    /// `budgets.per-run-cost-usd` (or restarts the target with a fresh
    /// cap). Phase 15 of `docs/design/2026-07-11-verified-swarm.md`.
    /// Takes no params.
    #[strum(serialize = "budget.reset")]
    BudgetReset,
    /// Subscribe to the daemon's live `DaemonEvent` stream. Phase 17 of
    /// `docs/design/2026-07-11-verified-swarm.md`. Unlike every other
    /// method this is LONG-LIVED: the daemon acks once, then streams
    /// event frames (plus periodic [`crate::WatchFrame::Heartbeat`]
    /// keepalives and, on broadcast lag, a typed
    /// [`crate::WatchFrame::Gap`] marker) until the client disconnects or
    /// the daemon shuts down. The server-side handling path is distinct
    /// from the one-shot request/response dispatch and is EXEMPT from the
    /// read-idle timeout. Takes no params (a live tail replays nothing).
    #[strum(serialize = "events.subscribe")]
    EventsSubscribe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Handshake(HandshakeParams),
    Status,
    PlanCreate(PlanCreateParams),
    RecordList(RecordListParams),
    RecordGet(RecordGetParams),
    DirectorChat(DirectorChatParams),
    PlanOverride(PlanOverrideParams),
    WorkOverride(WorkOverrideParams),
    DirectorStatus(DirectorStatusParams),
    BudgetReset,
    EventsSubscribe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeParams {
    pub protocol_version: u32,
    /// Client's resolved session-id. Additive field: older clients may
    /// omit it, in which case the daemon records the connection under
    /// its own daemon-boot session-id. Newer daemons treat `None` as
    /// equivalent to daemon-boot-session attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCreateParams {
    pub goal: String,
}

/// `director.chat` request. The operator submits a message routed
/// into the Director's per-iteration user prompt. Phase 8 of
/// `docs/design/2026-05-09-director-phase-2.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectorChatParams {
    /// Target Plan; the daemon validates this exists before persisting.
    pub plan_id: String,
    /// Operator's message. The daemon truncates anything beyond
    /// 4096 bytes (see `DIRECTOR_CHAT_MESSAGE_BYTE_CAP`) and appends a
    /// truncation marker so the LLM sees a bounded payload.
    pub message: String,
}

/// `director.chat` response: the newly persisted note's id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectorChatResult {
    pub note_id: String,
}

/// Hard cap on the post-truncation message stored in the note. Bounds
/// the LLM prompt-injection-by-volume surface; longer operator advice
/// should be summarized before submission.
pub const DIRECTOR_CHAT_MESSAGE_BYTE_CAP: usize = 4096;

/// `plan.override` request. The operator nominates a target FSM status
/// for the Plan; the daemon runs the override under `Role::Director`,
/// which is the only role permitted to edge from `Stalled`. Today the
/// only practical use is `Stalled -> Active` (revive an escalated Plan).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanOverrideParams {
    /// Target Plan id; daemon validates existence before the FSM call.
    pub plan_id: String,
    /// Target Plan status, lowercase string matching `PlanStatus`'s
    /// `Display` impl (e.g. `"active"`, `"stalled"`, `"complete"`).
    pub target_status: String,
}

/// `plan.override` response: the updated Plan record post-transition.
/// `Plan` does not implement `PartialEq`/`Eq`; wire round-trip is
/// asserted via byte stability in the seam tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanOverrideResult {
    pub plan: Plan,
}

/// `work.override` request. The operator nominates a target FSM status for
/// a single Work; the daemon runs the override under `Role::Director`.
/// Phase 18 of `docs/design/2026-07-11-verified-swarm.md`. The two edges
/// the daemon acts on specially are `Blocked -> Ready` (re-dispatch) and
/// `InProgress -> Blocked` (abort + reap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkOverrideParams {
    /// Target Work id; the daemon validates existence before the FSM call.
    pub work_id: String,
    /// Target Work status, lowercase string matching `WorkStatus`'s
    /// `Display` impl (e.g. `"ready"`, `"blocked"`).
    pub target_status: String,
}

/// `work.override` response: the updated Work record post-transition.
/// `Work` does not implement `PartialEq`/`Eq`; wire round-trip is asserted
/// via byte stability in the seam tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkOverrideResult {
    pub work: Work,
}

/// `director.status` request: name the Plan whose Director snapshot
/// the operator wants to read. Phase 2 follow-ups (Item 3) of
/// `docs/design/2026-05-12-director-phase-2-followups.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectorStatusParams {
    pub plan_id: String,
}

/// Wire form of `agents::DirectorStatusSnapshot`. The wire copy keeps
/// `mode` as a String (PascalCase: `Normal` / `Conservative` /
/// `NeedsOperator`) so `crates/ipc` does not need to depend on
/// `crates/agents`. The IPC handler converts the agents-side snapshot
/// into this struct at the response boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectorStatusSnapshot {
    pub mode: String,
    pub no_progress_streak: u32,
    pub same_action_streak: u32,
    pub iteration: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action_ts: Option<i64>,
    pub unread_note_count: usize,
    pub needs_operator_iters: u32,
}

/// `director.status` response. `snapshot: None` means the Plan exists
/// but no Director task is currently running for it (Plan is Stalled,
/// Complete, or transient pre-spawn). The CLI renders that as
/// "director: not running (plan is <status>)".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectorStatusResult {
    pub plan_id: String,
    pub plan_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<DirectorStatusSnapshot>,
}

impl TryFrom<&DaemonRequest> for Method {
    type Error = RpcError;
    fn try_from(req: &DaemonRequest) -> Result<Self, Self::Error> {
        use std::str::FromStr;
        let name = MethodName::from_str(&req.method).map_err(|_| RpcError::MethodNotFound(req.method.clone()))?;
        match name {
            MethodName::SystemHandshake => {
                let params: HandshakeParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::Handshake(params))
            }
            MethodName::SystemStatus => {
                if !req.params.is_null() && !matches!(&req.params, serde_json::Value::Object(m) if m.is_empty()) {
                    return Err(RpcError::InvalidParams("system.status takes no params".into()));
                }
                Ok(Method::Status)
            }
            MethodName::PlanCreate => {
                let params: PlanCreateParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::PlanCreate(params))
            }
            MethodName::RecordList => {
                let params: RecordListParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::RecordList(params))
            }
            MethodName::RecordGet => {
                let params: RecordGetParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::RecordGet(params))
            }
            MethodName::DirectorChat => {
                let params: DirectorChatParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::DirectorChat(params))
            }
            MethodName::PlanOverride => {
                let params: PlanOverrideParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::PlanOverride(params))
            }
            MethodName::WorkOverride => {
                let params: WorkOverrideParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::WorkOverride(params))
            }
            MethodName::DirectorStatus => {
                let params: DirectorStatusParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::DirectorStatus(params))
            }
            MethodName::BudgetReset => {
                if !req.params.is_null() && !matches!(&req.params, serde_json::Value::Object(m) if m.is_empty()) {
                    return Err(RpcError::InvalidParams("budget.reset takes no params".into()));
                }
                Ok(Method::BudgetReset)
            }
            MethodName::EventsSubscribe => {
                if !req.params.is_null() && !matches!(&req.params, serde_json::Value::Object(m) if m.is_empty()) {
                    return Err(RpcError::InvalidParams("events.subscribe takes no params".into()));
                }
                Ok(Method::EventsSubscribe)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeResult {
    pub protocol_version: u32,
    pub daemon_version: String,
}

/// Per-plan rollup embedded in `system.status`. Phase 16 of
/// `docs/design/2026-07-11-verified-swarm.md`: "fat status" gives an
/// operator the works-by-state / bundles-by-state / retry-spend / stuck
/// signal for every Plan in one call, instead of a separate `record.list`
/// per kind plus manual cross-referencing by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRollup {
    pub plan_id: String,
    /// Lowercase wire form matching `PlanStatus`'s `Display` impl (see
    /// `PlanOverrideParams::target_status`'s doc comment for the same
    /// convention).
    pub plan_status: String,
    /// Count of this Plan's Works, keyed by `WorkStatus`'s lowercase wire
    /// form (`"ready"`, `"inprogress"`, ...). `BTreeMap` keeps rendered
    /// YAML/JSON key order deterministic across runs.
    pub works_by_state: BTreeMap<String, u32>,
    /// Count of this Plan's Bundles (via their parent Works), keyed by
    /// `BundleStatus`'s lowercase wire form.
    pub bundles_by_state: BTreeMap<String, u32>,
    /// The live Director task's mode for this Plan
    /// (`"Normal"`/`"Conservative"`/`"NeedsOperator"`), mirroring
    /// `DirectorStatusResult.snapshot.mode`. `None` when no Director task
    /// is currently running for this Plan (Stalled, Complete, or a
    /// transient pre-spawn window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub director_mode: Option<String>,
    /// Sum of `attempt_count` across this Plan's Works: the total
    /// implementer/retry spend charged against the Plan's retry budget
    /// (`max_work_attempts`).
    pub total_attempts: u32,
    /// `true` when this Plan itself is `Stalled`, or its live Director
    /// mode is `NeedsOperator` -- the two states where an operator's
    /// intervention (not another automatic dispatch) is what unsticks it.
    pub stuck: bool,
}

// Not `Eq`: `cost_so_far_usd` is an `f64`, which has no total order (NaN).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResult {
    pub started_at: String,
    pub pid: u32,
    pub active_plans: u32,
    pub active_works: u32,
    /// Cumulative LLM cost in U.S. dollars for this daemon process's
    /// lifetime (`ProcessSnapshot::llm_cost_micros / 1_000_000`), priced
    /// per-call by each call's own model (Phase 4 of
    /// `docs/design/2026-07-11-verified-swarm.md` fixed this accuracy).
    /// Process-wide, not per-Plan: `ProcessSnapshot` has no per-Plan cost
    /// attribution today, so this is the daemon's total spend-so-far
    /// rather than a per-`PlanRollup` figure.
    pub cost_so_far_usd: f64,
    /// Phase 16 of `docs/design/2026-07-11-verified-swarm.md`: per-plan
    /// rollups (works/bundles by state, Director mode, retry-attempt
    /// total, stuck flag). One entry per Plan currently persisted in the
    /// target's taskstore, regardless of status.
    pub plans: Vec<PlanRollup>,
}

/// Success payload for `plan.create`: the newly persisted Plan record.
/// `Plan` does not implement `PartialEq`/`Eq` (`created_at`/`updated_at`
/// would make equality slippery), so neither does this wrapper. Wire
/// round-trip is asserted by encoding a known-good JSON string and
/// comparing byte stability in the seam tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCreateResult {
    pub plan: Plan,
}

/// `budget.reset` response. `was_tripped` tells the operator whether the
/// reset actually mattered: `true` means the per-run soft-pause had
/// fired and new implementer spawns are now unblocked; `false` means
/// the daemon was never tripped (a no-op reset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetResetResult {
    pub was_tripped: bool,
}
