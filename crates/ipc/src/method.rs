use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

use domain::Plan;

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
    DirectorStatus(DirectorStatusParams),
    BudgetReset,
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeResult {
    pub protocol_version: u32,
    pub daemon_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResult {
    pub started_at: String,
    pub pid: u32,
    pub active_plans: u32,
    pub active_works: u32,
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
