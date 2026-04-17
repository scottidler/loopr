use serde::{Deserialize, Serialize};

use crate::agents::{AgentEvent, AgentStatus};

/// Client → Daemon request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Structured RPC error returned inside a DaemonResponse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// Daemon → Client response to a specific request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// Daemon → Client unsolicited push event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonEvent {
    pub event: String,
    pub data: serde_json::Value,
}

/// Envelope for messages received by the client (responses or events).
/// Discriminated by field presence per the IPC Message Discrimination spec.
#[derive(Debug, Clone, PartialEq)]
pub enum IpcMessage {
    Response(DaemonResponse),
    Event(DaemonEvent),
}

// --- Standard RPC error codes ---

impl RpcError {
    // Code constants. Kept in sync with the constructor functions below.
    // Callers that need to inspect `resp.error.code` should match against these rather
    // than against raw integer literals.
    pub const CODE_METHOD_NOT_FOUND: i32 = -32601;
    pub const CODE_INVALID_PARAMS: i32 = -32602;
    pub const CODE_INTERNAL: i32 = -32603;
    pub const CODE_TRANSITION_REJECTED: i32 = -32000;
    pub const CODE_NOT_FOUND: i32 = -32001;
    pub const CODE_STALE_BUNDLE: i32 = -32002;
    pub const CODE_VALIDATION_REQUIRED: i32 = -32003;
    pub const CODE_POOL_EXHAUSTED: i32 = -32004;
    pub const CODE_PRECONDITION_FAILED: i32 = -32005;

    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: Self::CODE_METHOD_NOT_FOUND,
            message: format!("method not found: {method}"),
        }
    }

    pub fn invalid_params(detail: &str) -> Self {
        Self {
            code: Self::CODE_INVALID_PARAMS,
            message: format!("invalid params: {detail}"),
        }
    }

    pub fn internal(detail: &str) -> Self {
        Self {
            code: Self::CODE_INTERNAL,
            message: format!("internal error: {detail}"),
        }
    }

    pub fn transition_rejected(detail: &str) -> Self {
        Self {
            code: Self::CODE_TRANSITION_REJECTED,
            message: format!("transition rejected: {detail}"),
        }
    }

    pub fn not_found(collection: &str, id: &str) -> Self {
        Self {
            code: Self::CODE_NOT_FOUND,
            message: format!("not found: {collection}/{id}"),
        }
    }

    pub fn stale_bundle(base_tick_id: &str, latest_tick_id: &str) -> Self {
        Self {
            code: Self::CODE_STALE_BUNDLE,
            message: format!(
                "staleness guard: base_tick_id '{base_tick_id}' is behind latest Published Tick '{latest_tick_id}' - refresh worktree and re-propose"
            ),
        }
    }

    pub fn validation_required(collection: &str, id: &str) -> Self {
        Self {
            code: Self::CODE_VALIDATION_REQUIRED,
            message: format!(
                "Draft -> Active requires a passing validation report for {collection}/{id}. Run 'validator.validate' first."
            ),
        }
    }

    pub fn pool_exhausted(detail: &str) -> Self {
        Self {
            code: Self::CODE_POOL_EXHAUSTED,
            message: format!("pool exhausted: {detail}"),
        }
    }

    pub fn precondition_failed(detail: &str) -> Self {
        Self {
            code: Self::CODE_PRECONDITION_FAILED,
            message: format!("precondition failed: {detail}"),
        }
    }
}

// --- Convenience constructors ---

impl DaemonRequest {
    pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }
}

impl DaemonResponse {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, error: RpcError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

// --- Reconciliation reason constants (used in reconciliation.fixed / reconciliation.failed events) ---

// Recoverable reasons
pub const REASON_MISSING_HANDLE: &str = "MissingHandle";
pub const REASON_HANDLE_FINISHED: &str = "HandleFinished";
pub const REASON_SESSION_TIMEOUT: &str = "SessionTimeout";
pub const REASON_HOLDER_TERMINAL: &str = "HolderTerminal";
pub const REASON_HOLDER_WORK_DONE: &str = "HolderWorkDone";
pub const REASON_LOCK_EXPIRED: &str = "LockExpired";
pub const REASON_STALE_WORKTREE: &str = "StaleWorktree";
pub const REASON_MISSING_BRANCH: &str = "MissingBranch";

// Catastrophic reasons
pub const REASON_SHA_UNREACHABLE: &str = "ShaUnreachable";
pub const REASON_SHA_MISSING: &str = "ShaMissing";
pub const REASON_MERGE_NOT_ANCESTOR: &str = "MergeNotAncestor";

impl DaemonEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    pub fn transition_completed(collection: &str, id: &str, from: &str, to: &str, role: &str) -> Self {
        Self::new(
            "transition.completed",
            serde_json::json!({
                "collection": collection,
                "id": id,
                "from": from,
                "to": to,
                "role": role,
            }),
        )
    }

    pub fn record_created(collection: &str, id: &str) -> Self {
        Self::new(
            "record.created",
            serde_json::json!({ "collection": collection, "id": id }),
        )
    }

    pub fn record_updated(collection: &str, id: &str) -> Self {
        Self::new(
            "record.updated",
            serde_json::json!({ "collection": collection, "id": id }),
        )
    }

    /// Emitted after a Tick is published and its bundles are Merged.
    /// Implementers drain this to rebase their worktrees against the updated integration branch.
    pub fn bundle_merged(tick_id: &str, integration_sha: &str, merged_bundle_ids: &[String]) -> Self {
        Self::new(
            "bundle.merged",
            serde_json::json!({
                "tick-id": tick_id,
                "integration-sha": integration_sha,
                "merged_bundle_ids": merged_bundle_ids,
            }),
        )
    }

    pub fn tick_published(tick_id: &str, sha: &str) -> Self {
        Self::new(
            "tick.published",
            serde_json::json!({
                "tick-id": tick_id,
                "sha": sha,
            }),
        )
    }

    pub fn tick_validation_failed(tick_id: &str, reason: &str) -> Self {
        Self::new(
            "tick.validation_failed",
            serde_json::json!({
                "tick-id": tick_id,
                "reason": reason,
            }),
        )
    }

    pub fn bundle_rejected_stale(work_id: &str, base_tick_id: &str, latest_tick_id: &str) -> Self {
        Self::new(
            "bundle.rejected_stale",
            serde_json::json!({
                "bundle_work_id": work_id,
                "base-tick-id": base_tick_id,
                "latest_tick_id": latest_tick_id,
            }),
        )
    }

    pub fn agent_status_changed(session_id: &str, status: AgentStatus) -> Self {
        let event = AgentEvent::StatusChange {
            session_id: session_id.to_string(),
            status,
            error: None,
        };
        Self::new("agent.status_changed", serde_json::to_value(event).unwrap_or_default())
    }

    pub fn agent_status_failed(session_id: &str, error: Option<String>) -> Self {
        let event = AgentEvent::StatusChange {
            session_id: session_id.to_string(),
            status: AgentStatus::Failed,
            error,
        };
        Self::new("agent.status_changed", serde_json::to_value(event).unwrap_or_default())
    }

    pub fn agent_tool_started(session_id: &str, tool: &str) -> Self {
        let event = AgentEvent::ToolStarted {
            session_id: session_id.to_string(),
            tool: tool.to_string(),
        };
        Self::new("agent.tool_started", serde_json::to_value(event).unwrap_or_default())
    }

    pub fn agent_tool_completed(session_id: &str, tool: &str, exit_code: i32, duration_ms: u64) -> Self {
        let event = AgentEvent::ToolCompleted {
            session_id: session_id.to_string(),
            tool: tool.to_string(),
            exit_code,
            duration_ms,
        };
        Self::new("agent.tool_completed", serde_json::to_value(event).unwrap_or_default())
    }

    pub fn agent_action_completed(session_id: &str, action_summary: &str) -> Self {
        let event = AgentEvent::ActionCompleted {
            session_id: session_id.to_string(),
            action_summary: action_summary.to_string(),
        };
        Self::new(
            "agent.action_completed",
            serde_json::to_value(event).unwrap_or_default(),
        )
    }

    pub fn agent_iteration_completed(session_id: &str, iteration: u32, summary: &str) -> Self {
        let event = AgentEvent::IterationCompleted {
            session_id: session_id.to_string(),
            iteration,
            summary: summary.to_string(),
        };
        Self::new(
            "agent.iteration_completed",
            serde_json::to_value(event).unwrap_or_default(),
        )
    }

    pub fn agent_staleness_detected(session_id: &str, new_tick_id: &str) -> Self {
        let event = AgentEvent::StalenessDetected {
            session_id: session_id.to_string(),
            new_tick_id: new_tick_id.to_string(),
        };
        Self::new(
            "agent.staleness_detected",
            serde_json::to_value(event).unwrap_or_default(),
        )
    }

    pub fn record_deleted(collection: &str, id: &str) -> Self {
        Self::new(
            "record.deleted",
            serde_json::json!({ "collection": collection, "id": id }),
        )
    }

    pub fn transition_rejected(collection: &str, id: &str, from: &str, to: &str, role: &str, reason: &str) -> Self {
        Self::new(
            "transition.rejected",
            serde_json::json!({
                "collection": collection,
                "id": id,
                "from": from,
                "to": to,
                "role": role,
                "reason": reason,
            }),
        )
    }

    pub fn validation_started(tick_id: &str) -> Self {
        Self::new("validation.started", serde_json::json!({ "tick-id": tick_id }))
    }

    pub fn validation_completed(tick_id: &str, success: bool, log: &str) -> Self {
        Self::new(
            "validation.completed",
            serde_json::json!({ "tick-id": tick_id, "success": success, "log": log }),
        )
    }

    pub fn agent_timing_info(session_id: &str, label: &str, detail: &str) -> Self {
        let event = AgentEvent::TimingInfo {
            session_id: session_id.to_string(),
            label: label.to_string(),
            detail: detail.to_string(),
        };
        Self::new("agent.timing_info", serde_json::to_value(event).unwrap_or_default())
    }

    pub fn learning_policy_contradicted(learning_id: &str) -> Self {
        Self::new(
            "learning.policy_contradicted",
            serde_json::json!({ "learning_id": learning_id }),
        )
    }

    /// Emitted when reconciliation detects and fixes a state fracture.
    pub fn reconciled(collection: &str, id: &str, from: &str, to: &str, reason: &str) -> Self {
        Self::new(
            "reconciliation.fixed",
            serde_json::json!({
                "collection": collection,
                "id": id,
                "from": from,
                "to": to,
                "reason": reason,
            }),
        )
    }

    /// Emitted when reconciliation detects a catastrophic fracture requiring manual intervention.
    pub fn reconciliation_failed(collection: &str, id: &str, status: &str, reason: &str) -> Self {
        Self::new(
            "reconciliation.failed",
            serde_json::json!({
                "collection": collection,
                "id": id,
                "status": status,
                "reason": reason,
                "severity": "catastrophic",
            }),
        )
    }

    /// Emitted whenever the Director flips between modes.
    pub fn director_mode_changed(session_id: &str, mode: &str, plan_id: Option<&str>) -> Self {
        Self::new(
            "director.mode_changed",
            serde_json::json!({
                "session-id": session_id,
                "mode": mode,
                "plan-id": plan_id,
            }),
        )
    }

    /// Emitted by the Director heartbeat when no broadcast events have arrived
    /// during Monitoring for longer than the stall threshold.
    pub fn director_stall_detected(session_id: &str, plan_id: Option<&str>, idle_secs: u64) -> Self {
        Self::new(
            "director.stall_detected",
            serde_json::json!({
                "session-id": session_id,
                "plan-id": plan_id,
                "idle_secs": idle_secs,
            }),
        )
    }
}

/// Parse a raw JSON line into an IpcMessage.
/// Discrimination: if "method" field present → it's a request (error on client side).
/// If "event" field present → DaemonEvent. Otherwise → DaemonResponse.
impl IpcMessage {
    pub fn from_json(line: &str) -> Result<Self, serde_json::Error> {
        let v: serde_json::Value = serde_json::from_str(line)?;
        if v.get("event").is_some() {
            let event: DaemonEvent = serde_json::from_value(v)?;
            Ok(IpcMessage::Event(event))
        } else {
            let resp: DaemonResponse = serde_json::from_value(v)?;
            Ok(IpcMessage::Response(resp))
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_request_serde_roundtrip() {
        let req = DaemonRequest::new(1, "work.get", json!({"id": "abc123"}));
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, parsed);
    }

    #[test]
    fn test_request_has_method_field() {
        let req = DaemonRequest::new(42, "plan.create", json!({"title": "test"}));
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(v["method"], "plan.create");
        assert_eq!(v["id"], 42);
    }

    #[test]
    fn test_request_default_params() {
        let json = r#"{"id": 1, "method": "system.status"}"#;
        let req: DaemonRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.params, serde_json::Value::Null);
    }

    #[test]
    fn test_response_ok_serde_roundtrip() {
        let resp = DaemonResponse::ok(1, json!({"status": "Draft"}));
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
        assert!(!resp.is_error());
    }

    #[test]
    fn test_response_err_serde_roundtrip() {
        let resp = DaemonResponse::err(2, RpcError::method_not_found("foo.bar"));
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, parsed);
        assert!(resp.is_error());
    }

    #[test]
    fn test_response_ok_omits_error_field() {
        let resp = DaemonResponse::ok(1, json!("data"));
        let v: serde_json::Value = serde_json::to_value(&resp).unwrap();
        assert!(v.get("error").is_none());
        assert!(v.get("result").is_some());
    }

    #[test]
    fn test_response_err_omits_result_field() {
        let resp = DaemonResponse::err(1, RpcError::internal("boom"));
        let v: serde_json::Value = serde_json::to_value(&resp).unwrap();
        assert!(v.get("result").is_none());
        assert!(v.get("error").is_some());
    }

    #[test]
    fn test_event_serde_roundtrip() {
        let event = DaemonEvent::new("tick.published", json!({"tick-id": "t1", "sha": "abc"}));
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DaemonEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn test_event_transition_completed() {
        let event = DaemonEvent::transition_completed("work", "wi1", "Draft", "Ready", "Coordinator");
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "work");
        assert_eq!(event.data["from"], "Draft");
        assert_eq!(event.data["to"], "Ready");
    }

    #[test]
    fn test_event_record_created() {
        let event = DaemonEvent::record_created("plan", "p1");
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "plan");
        assert_eq!(event.data["id"], "p1");
    }

    #[test]
    fn test_rpc_error_codes() {
        assert_eq!(RpcError::method_not_found("x").code, -32601);
        assert_eq!(RpcError::invalid_params("x").code, -32602);
        assert_eq!(RpcError::internal("x").code, -32603);
        assert_eq!(RpcError::transition_rejected("x").code, -32000);
        assert_eq!(RpcError::not_found("plan", "p1").code, -32001);
        assert!(RpcError::not_found("plan", "p1").message.contains("plan/p1"));
        assert_eq!(RpcError::validation_required("plan", "p1").code, -32003);
        assert!(RpcError::validation_required("plan", "p1").message.contains("plan/p1"));
        assert!(
            RpcError::validation_required("plan", "p1")
                .message
                .contains("validator.validate")
        );
        assert_eq!(RpcError::pool_exhausted("x").code, -32004);
        assert!(RpcError::pool_exhausted("test detail").message.contains("test detail"));
    }

    #[test]
    fn test_ipc_message_discriminate_response() {
        let line = r#"{"id": 1, "result": {"status": "ok"}}"#;
        let msg = IpcMessage::from_json(line).unwrap();
        match msg {
            IpcMessage::Response(r) => {
                assert_eq!(r.id, 1);
                assert!(!r.is_error());
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn test_ipc_message_discriminate_event() {
        let line = r#"{"event": "tick.published", "data": {"sha": "abc"}}"#;
        let msg = IpcMessage::from_json(line).unwrap();
        match msg {
            IpcMessage::Event(e) => {
                assert_eq!(e.event, "tick.published");
                assert_eq!(e.data["sha"], "abc");
            }
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn test_ipc_message_discriminate_error_response() {
        let line = r#"{"id": 5, "error": {"code": -32601, "message": "method not found: bad"}}"#;
        let msg = IpcMessage::from_json(line).unwrap();
        match msg {
            IpcMessage::Response(r) => {
                assert_eq!(r.id, 5);
                assert!(r.is_error());
                assert_eq!(r.error.unwrap().code, -32601);
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn test_ipc_message_invalid_json() {
        let result = IpcMessage::from_json("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_event_record_deleted() {
        let event = DaemonEvent::record_deleted("plans", "p1");
        assert_eq!(event.event, "record.deleted");
        assert_eq!(event.data["collection"], "plans");
        assert_eq!(event.data["id"], "p1");
    }

    #[test]
    fn test_event_transition_rejected() {
        let event =
            DaemonEvent::transition_rejected("plans", "p1", "Draft", "Active", "Coordinator", "validation required");
        assert_eq!(event.event, "transition.rejected");
        assert_eq!(event.data["collection"], "plans");
        assert_eq!(event.data["id"], "p1");
        assert_eq!(event.data["from"], "Draft");
        assert_eq!(event.data["to"], "Active");
        assert_eq!(event.data["role"], "Coordinator");
        assert_eq!(event.data["reason"], "validation required");
    }

    #[test]
    fn test_event_validation_started() {
        let event = DaemonEvent::validation_started("t1");
        assert_eq!(event.event, "validation.started");
        assert_eq!(event.data["tick-id"], "t1");
    }

    #[test]
    fn test_event_validation_completed() {
        let event = DaemonEvent::validation_completed("t1", true, "all passed");
        assert_eq!(event.event, "validation.completed");
        assert_eq!(event.data["tick-id"], "t1");
        assert_eq!(event.data["success"], true);
        assert_eq!(event.data["log"], "all passed");
    }

    #[test]
    fn test_event_learning_policy_contradicted() {
        let event = DaemonEvent::learning_policy_contradicted("l1");
        assert_eq!(event.event, "learning.policy_contradicted");
        assert_eq!(event.data["learning_id"], "l1");
    }

    #[test]
    fn test_event_agent_timing_info() {
        let event = DaemonEvent::agent_timing_info("chat-1", "iter 0", "total=3204ms llm=2891ms tools=298ms");
        assert_eq!(event.event, "agent.timing_info");
        let data: AgentEvent = serde_json::from_value(event.data).unwrap();
        match data {
            AgentEvent::TimingInfo {
                session_id,
                label,
                detail,
            } => {
                assert_eq!(session_id, "chat-1");
                assert_eq!(label, "iter 0");
                assert!(detail.contains("total=3204ms"));
            }
            _ => panic!("expected TimingInfo"),
        }
    }

    #[test]
    fn test_event_agent_timing_info_roundtrip() {
        let event = DaemonEvent::agent_timing_info("s1", "loop_complete", "total=4360ms iterations=2");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DaemonEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event, "agent.timing_info");
        assert_eq!(event, parsed);
    }

    #[test]
    fn test_event_reconciled() {
        let event = DaemonEvent::reconciled("work", "wi-1", "InProgress", "Blocked", REASON_MISSING_HANDLE);
        assert_eq!(event.event, "reconciliation.fixed");
        assert_eq!(event.data["collection"], "work");
        assert_eq!(event.data["id"], "wi-1");
        assert_eq!(event.data["from"], "InProgress");
        assert_eq!(event.data["to"], "Blocked");
        assert_eq!(event.data["reason"], REASON_MISSING_HANDLE);
    }

    #[test]
    fn test_event_reconciliation_failed() {
        let event = DaemonEvent::reconciliation_failed("tick", "tk-1", "Published", REASON_SHA_UNREACHABLE);
        assert_eq!(event.event, "reconciliation.failed");
        assert_eq!(event.data["collection"], "tick");
        assert_eq!(event.data["id"], "tk-1");
        assert_eq!(event.data["status"], "Published");
        assert_eq!(event.data["reason"], REASON_SHA_UNREACHABLE);
        assert_eq!(event.data["severity"], "catastrophic");
    }

    #[test]
    fn test_reason_constants() {
        assert_eq!(REASON_MISSING_HANDLE, "MissingHandle");
        assert_eq!(REASON_HANDLE_FINISHED, "HandleFinished");
        assert_eq!(REASON_SESSION_TIMEOUT, "SessionTimeout");
        assert_eq!(REASON_HOLDER_TERMINAL, "HolderTerminal");
        assert_eq!(REASON_HOLDER_WORK_DONE, "HolderWorkDone");
        assert_eq!(REASON_LOCK_EXPIRED, "LockExpired");
        assert_eq!(REASON_STALE_WORKTREE, "StaleWorktree");
        assert_eq!(REASON_MISSING_BRANCH, "MissingBranch");
        assert_eq!(REASON_SHA_UNREACHABLE, "ShaUnreachable");
        assert_eq!(REASON_SHA_MISSING, "ShaMissing");
        assert_eq!(REASON_MERGE_NOT_ANCESTOR, "MergeNotAncestor");
    }
}
