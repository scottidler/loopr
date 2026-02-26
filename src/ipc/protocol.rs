use serde::{Deserialize, Serialize};

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
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
        }
    }

    pub fn invalid_params(detail: &str) -> Self {
        Self {
            code: -32602,
            message: format!("invalid params: {detail}"),
        }
    }

    pub fn internal(detail: &str) -> Self {
        Self {
            code: -32603,
            message: format!("internal error: {detail}"),
        }
    }

    pub fn transition_rejected(detail: &str) -> Self {
        Self {
            code: -32000,
            message: format!("transition rejected: {detail}"),
        }
    }

    pub fn not_found(collection: &str, id: &str) -> Self {
        Self {
            code: -32001,
            message: format!("not found: {collection}/{id}"),
        }
    }

    pub fn stale_bundle(base_tick_id: &str, latest_tick_id: &str) -> Self {
        Self {
            code: -32002,
            message: format!(
                "staleness guard: base_tick_id '{base_tick_id}' is behind latest Published Tick '{latest_tick_id}' — refresh worktree and re-propose"
            ),
        }
    }

    pub fn validation_required(collection: &str, id: &str) -> Self {
        Self {
            code: -32003,
            message: format!(
                "Draft → Active requires a passing validation report for {collection}/{id}. Run 'validator.validate' first."
            ),
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

    pub fn tick_published(tick_id: &str, sha: &str) -> Self {
        Self::new(
            "tick.published",
            serde_json::json!({
                "tick_id": tick_id,
                "sha": sha,
            }),
        )
    }

    pub fn tick_validation_failed(tick_id: &str, reason: &str) -> Self {
        Self::new(
            "tick.validation_failed",
            serde_json::json!({
                "tick_id": tick_id,
                "reason": reason,
            }),
        )
    }

    pub fn bundle_rejected_stale(work_item_id: &str, base_tick_id: &str, latest_tick_id: &str) -> Self {
        Self::new(
            "bundle.rejected_stale",
            serde_json::json!({
                "bundle_work_item_id": work_item_id,
                "base_tick_id": base_tick_id,
                "latest_tick_id": latest_tick_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_request_serde_roundtrip() {
        let req = DaemonRequest::new(1, "work_item.get", json!({"id": "abc123"}));
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
        let event = DaemonEvent::new("tick.published", json!({"tick_id": "t1", "sha": "abc"}));
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DaemonEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, parsed);
    }

    #[test]
    fn test_event_transition_completed() {
        let event = DaemonEvent::transition_completed("work_item", "wi1", "Draft", "Ready", "Coordinator");
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "work_item");
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
}
