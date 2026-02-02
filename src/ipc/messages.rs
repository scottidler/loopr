//! IPC message types for TUI ↔ Daemon communication.
//!
//! Uses JSON Lines (newline-delimited JSON) over Unix stream socket.
//! Message schema uses familiar field names (id, method, params, result, error)
//! but does NOT implement JSON-RPC 2.0 specification.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::Loop;

/// Request sent from TUI to Daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRequest {
    /// Unique request ID for correlating responses.
    pub id: u64,
    /// Method name (e.g., "loop.list", "chat.send").
    pub method: String,
    /// Method parameters as JSON value.
    #[serde(default)]
    pub params: Value,
}

impl DaemonRequest {
    /// Create a new request with the given method and params.
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }

    /// Create a request with no parameters.
    pub fn no_params(id: u64, method: impl Into<String>) -> Self {
        Self::new(id, method, Value::Object(Default::default()))
    }
}

/// Response sent from Daemon to TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponse {
    /// Request ID this response corresponds to.
    pub id: u64,
    /// Result value on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error details on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DaemonError>,
}

impl DaemonResponse {
    /// Create a success response.
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(id: u64, error: DaemonError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }

    /// Check if this response indicates success.
    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }
}

/// Error details in a daemon response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonError {
    /// Error code.
    pub code: i32,
    /// Human-readable error message.
    pub message: String,
    /// Additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl DaemonError {
    /// Create a new error.
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Create an error with additional data.
    pub fn with_data(code: i32, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    /// Parse error (-32700).
    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PARSE_ERROR, message)
    }

    /// Invalid request error (-32600).
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_REQUEST, message)
    }

    /// Method not found error (-32601).
    pub fn method_not_found(method: impl Into<String>) -> Self {
        Self::new(
            ErrorCode::METHOD_NOT_FOUND,
            format!("Unknown method: {}", method.into()),
        )
    }

    /// Invalid params error (-32602).
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_PARAMS, message)
    }

    /// Internal error (-32603).
    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INTERNAL_ERROR, message)
    }

    /// Loop not found error (1001).
    pub fn loop_not_found(id: impl Into<String>) -> Self {
        Self::new(ErrorCode::LOOP_NOT_FOUND, format!("Loop not found: {}", id.into()))
    }

    /// Invalid state error (1002).
    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INVALID_STATE, message)
    }

    /// Version mismatch error (1004).
    pub fn version_mismatch(client_version: &str, daemon_version: &str) -> Self {
        Self::with_data(
            ErrorCode::VERSION_MISMATCH,
            format!("Version mismatch: client={}, daemon={}", client_version, daemon_version),
            serde_json::json!({
                "client_version": client_version,
                "daemon_version": daemon_version,
            }),
        )
    }
}

/// Standard error codes.
pub struct ErrorCode;

impl ErrorCode {
    /// Invalid JSON.
    pub const PARSE_ERROR: i32 = -32700;
    /// Invalid request object.
    pub const INVALID_REQUEST: i32 = -32600;
    /// Unknown method.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Invalid parameters.
    pub const INVALID_PARAMS: i32 = -32602;
    /// Internal daemon error.
    pub const INTERNAL_ERROR: i32 = -32603;
    /// Loop ID doesn't exist.
    pub const LOOP_NOT_FOUND: i32 = 1001;
    /// Loop in wrong state for action.
    pub const INVALID_STATE: i32 = 1002;
    /// Action not permitted.
    pub const UNAUTHORIZED: i32 = 1003;
    /// Client/daemon version mismatch.
    pub const VERSION_MISMATCH: i32 = 1004;
}

/// Push event sent from Daemon to TUI (no request ID).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonEvent {
    /// Event type (e.g., "loop.created", "chat.chunk").
    pub event: String,
    /// Event data.
    pub data: Value,
}

impl DaemonEvent {
    /// Create a new event.
    pub fn new(event: impl Into<String>, data: Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// Create a loop.created event.
    pub fn loop_created(loop_record: &Loop) -> Self {
        Self::new("loop.created", serde_json::to_value(loop_record).unwrap_or(Value::Null))
    }

    /// Create a loop.updated event.
    pub fn loop_updated(loop_record: &Loop) -> Self {
        Self::new("loop.updated", serde_json::to_value(loop_record).unwrap_or(Value::Null))
    }

    /// Create a loop.iteration event.
    pub fn loop_iteration(loop_id: &str, iteration: u32, passed: bool) -> Self {
        Self::new(
            "loop.iteration",
            serde_json::json!({
                "id": loop_id,
                "iteration": iteration,
                "passed": passed
            }),
        )
    }

    /// Create a chat.chunk event.
    pub fn chat_chunk(text: &str, done: bool) -> Self {
        Self::new(
            "chat.chunk",
            serde_json::json!({
                "text": text,
                "done": done
            }),
        )
    }

    /// Create a chat.tool_call event.
    pub fn chat_tool_call(tool: &str, input: Value) -> Self {
        Self::new(
            "chat.tool_call",
            serde_json::json!({
                "tool": tool,
                "input": input
            }),
        )
    }

    /// Create a chat.tool_result event.
    pub fn chat_tool_result(tool: &str, output: &str) -> Self {
        Self::new(
            "chat.tool_result",
            serde_json::json!({
                "tool": tool,
                "output": output
            }),
        )
    }

    /// Create a plan.awaiting_approval event.
    pub fn plan_awaiting_approval(id: &str, content: &str, specs: Vec<String>) -> Self {
        Self::new(
            "plan.awaiting_approval",
            serde_json::json!({
                "id": id,
                "content": content,
                "specs": specs
            }),
        )
    }

    /// Create a plan.approved event.
    pub fn plan_approved(id: &str, specs_spawned: u32) -> Self {
        Self::new(
            "plan.approved",
            serde_json::json!({
                "id": id,
                "specs_spawned": specs_spawned
            }),
        )
    }

    /// Create a plan.rejected event.
    pub fn plan_rejected(id: &str, reason: Option<&str>) -> Self {
        Self::new(
            "plan.rejected",
            serde_json::json!({
                "id": id,
                "reason": reason
            }),
        )
    }
}

/// IPC message enum for unified handling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IpcMessage {
    /// Request from TUI.
    Request(DaemonRequest),
    /// Response from Daemon.
    Response(DaemonResponse),
    /// Push event from Daemon.
    Event(DaemonEvent),
}

/// Known method names as constants.
pub struct Methods;

impl Methods {
    // Connection & Handshake
    pub const INITIALIZE: &'static str = "initialize";
    pub const CONNECT: &'static str = "connect";
    pub const DISCONNECT: &'static str = "disconnect";
    pub const PING: &'static str = "ping";

    // Chat
    pub const CHAT_SEND: &'static str = "chat.send";
    pub const CHAT_CANCEL: &'static str = "chat.cancel";
    pub const CHAT_CLEAR: &'static str = "chat.clear";

    // Loops
    pub const LOOP_LIST: &'static str = "loop.list";
    pub const LOOP_GET: &'static str = "loop.get";
    pub const LOOP_CREATE_PLAN: &'static str = "loop.create_plan";
    pub const LOOP_START: &'static str = "loop.start";
    pub const LOOP_PAUSE: &'static str = "loop.pause";
    pub const LOOP_RESUME: &'static str = "loop.resume";
    pub const LOOP_CANCEL: &'static str = "loop.cancel";
    pub const LOOP_DELETE: &'static str = "loop.delete";

    // Plan approval
    pub const PLAN_APPROVE: &'static str = "plan.approve";
    pub const PLAN_REJECT: &'static str = "plan.reject";
    pub const PLAN_ITERATE: &'static str = "plan.iterate";
    pub const PLAN_GET_PREVIEW: &'static str = "plan.get_preview";

    // Metrics
    pub const METRICS_GET: &'static str = "metrics.get";
}

/// Known event names as constants.
pub struct Events;

impl Events {
    pub const CHAT_CHUNK: &'static str = "chat.chunk";
    pub const CHAT_TOOL_CALL: &'static str = "chat.tool_call";
    pub const CHAT_TOOL_RESULT: &'static str = "chat.tool_result";
    pub const LOOP_CREATED: &'static str = "loop.created";
    pub const LOOP_UPDATED: &'static str = "loop.updated";
    pub const LOOP_ITERATION: &'static str = "loop.iteration";
    pub const LOOP_ARTIFACT: &'static str = "loop.artifact";
    pub const PLAN_AWAITING_APPROVAL: &'static str = "plan.awaiting_approval";
    pub const PLAN_APPROVED: &'static str = "plan.approved";
    pub const PLAN_REJECTED: &'static str = "plan.rejected";
    pub const METRICS_UPDATE: &'static str = "metrics.update";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_request_new() {
        let req = DaemonRequest::new(1, "test.method", serde_json::json!({"key": "value"}));
        assert_eq!(req.id, 1);
        assert_eq!(req.method, "test.method");
        assert_eq!(req.params["key"], "value");
    }

    #[test]
    fn test_daemon_request_no_params() {
        let req = DaemonRequest::no_params(42, "ping");
        assert_eq!(req.id, 42);
        assert_eq!(req.method, "ping");
        assert!(req.params.is_object());
    }

    #[test]
    fn test_daemon_request_serialize() {
        let req = DaemonRequest::new(1, "loop.list", serde_json::json!({}));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"method\":\"loop.list\""));
    }

    #[test]
    fn test_daemon_response_success() {
        let resp = DaemonResponse::success(1, serde_json::json!({"loops": []}));
        assert!(resp.is_success());
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_daemon_response_error() {
        let err = DaemonError::loop_not_found("test-id");
        let resp = DaemonResponse::error(1, err);
        assert!(!resp.is_success());
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
    }

    #[test]
    fn test_daemon_error_codes() {
        assert_eq!(DaemonError::parse_error("test").code, ErrorCode::PARSE_ERROR);
        assert_eq!(DaemonError::invalid_request("test").code, ErrorCode::INVALID_REQUEST);
        assert_eq!(DaemonError::method_not_found("test").code, ErrorCode::METHOD_NOT_FOUND);
        assert_eq!(DaemonError::invalid_params("test").code, ErrorCode::INVALID_PARAMS);
        assert_eq!(DaemonError::internal_error("test").code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(DaemonError::loop_not_found("test").code, ErrorCode::LOOP_NOT_FOUND);
        assert_eq!(DaemonError::invalid_state("test").code, ErrorCode::INVALID_STATE);
        assert_eq!(
            DaemonError::version_mismatch("v1.0", "v2.0").code,
            ErrorCode::VERSION_MISMATCH
        );
    }

    #[test]
    fn test_daemon_error_version_mismatch() {
        let err = DaemonError::version_mismatch("v1.0.0", "v2.0.0");

        // Check error code
        assert_eq!(err.code, ErrorCode::VERSION_MISMATCH);

        // Check message contains both versions
        assert!(err.message.contains("v1.0.0"), "Message should contain client version");
        assert!(err.message.contains("v2.0.0"), "Message should contain daemon version");
        assert!(err.message.contains("mismatch"), "Message should mention mismatch");

        // Check data contains structured version info
        let data = err.data.expect("version_mismatch should include data");
        assert_eq!(data["client_version"], "v1.0.0");
        assert_eq!(data["daemon_version"], "v2.0.0");
    }

    #[test]
    fn test_version_mismatch_error_code_value() {
        // Verify VERSION_MISMATCH is in the application-specific range (1000+)
        // If it equals 1004, it's implicitly > 1000
        assert_eq!(ErrorCode::VERSION_MISMATCH, 1004);
    }

    #[test]
    fn test_daemon_error_with_data() {
        let err = DaemonError::with_data(100, "custom error", serde_json::json!({"detail": "info"}));
        assert_eq!(err.code, 100);
        assert_eq!(err.message, "custom error");
        assert!(err.data.is_some());
    }

    #[test]
    fn test_daemon_event_new() {
        let event = DaemonEvent::new("test.event", serde_json::json!({"key": "value"}));
        assert_eq!(event.event, "test.event");
        assert_eq!(event.data["key"], "value");
    }

    #[test]
    fn test_daemon_event_chat_chunk() {
        let event = DaemonEvent::chat_chunk("Hello", false);
        assert_eq!(event.event, "chat.chunk");
        assert_eq!(event.data["text"], "Hello");
        assert_eq!(event.data["done"], false);
    }

    #[test]
    fn test_daemon_event_chat_chunk_done() {
        let event = DaemonEvent::chat_chunk("World", true);
        assert_eq!(event.data["done"], true);
    }

    #[test]
    fn test_daemon_event_chat_tool_call() {
        let event = DaemonEvent::chat_tool_call("read_file", serde_json::json!({"path": "test.rs"}));
        assert_eq!(event.event, "chat.tool_call");
        assert_eq!(event.data["tool"], "read_file");
        assert_eq!(event.data["input"]["path"], "test.rs");
    }

    #[test]
    fn test_daemon_event_chat_tool_result() {
        let event = DaemonEvent::chat_tool_result("read_file", "file contents");
        assert_eq!(event.event, "chat.tool_result");
        assert_eq!(event.data["tool"], "read_file");
        assert_eq!(event.data["output"], "file contents");
    }

    #[test]
    fn test_daemon_event_loop_iteration() {
        let event = DaemonEvent::loop_iteration("loop-123", 5, true);
        assert_eq!(event.event, "loop.iteration");
        assert_eq!(event.data["id"], "loop-123");
        assert_eq!(event.data["iteration"], 5);
        assert_eq!(event.data["passed"], true);
    }

    #[test]
    fn test_daemon_event_plan_awaiting_approval() {
        let event = DaemonEvent::plan_awaiting_approval(
            "plan-1",
            "# Plan content",
            vec!["spec-a".to_string(), "spec-b".to_string()],
        );
        assert_eq!(event.event, "plan.awaiting_approval");
        assert_eq!(event.data["id"], "plan-1");
        assert_eq!(event.data["content"], "# Plan content");
        assert_eq!(event.data["specs"][0], "spec-a");
    }

    #[test]
    fn test_daemon_event_plan_approved() {
        let event = DaemonEvent::plan_approved("plan-1", 3);
        assert_eq!(event.event, "plan.approved");
        assert_eq!(event.data["id"], "plan-1");
        assert_eq!(event.data["specs_spawned"], 3);
    }

    #[test]
    fn test_daemon_event_plan_rejected() {
        let event = DaemonEvent::plan_rejected("plan-1", Some("Not detailed enough"));
        assert_eq!(event.event, "plan.rejected");
        assert_eq!(event.data["id"], "plan-1");
        assert_eq!(event.data["reason"], "Not detailed enough");
    }

    #[test]
    fn test_daemon_event_plan_rejected_no_reason() {
        let event = DaemonEvent::plan_rejected("plan-1", None);
        assert_eq!(event.data["reason"], Value::Null);
    }

    #[test]
    fn test_methods_constants() {
        assert_eq!(Methods::CONNECT, "connect");
        assert_eq!(Methods::LOOP_LIST, "loop.list");
        assert_eq!(Methods::PLAN_APPROVE, "plan.approve");
    }

    #[test]
    fn test_events_constants() {
        assert_eq!(Events::CHAT_CHUNK, "chat.chunk");
        assert_eq!(Events::LOOP_CREATED, "loop.created");
        assert_eq!(Events::PLAN_APPROVED, "plan.approved");
    }

    #[test]
    fn test_request_roundtrip() {
        let req = DaemonRequest::new(123, "loop.get", serde_json::json!({"id": "loop-456"}));
        let json = serde_json::to_string(&req).unwrap();
        let parsed: DaemonRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 123);
        assert_eq!(parsed.method, "loop.get");
        assert_eq!(parsed.params["id"], "loop-456");
    }

    #[test]
    fn test_response_roundtrip() {
        let resp = DaemonResponse::success(1, serde_json::json!({"status": "ok"}));
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 1);
        assert!(parsed.is_success());
    }

    #[test]
    fn test_event_roundtrip() {
        let event = DaemonEvent::chat_chunk("test", false);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: DaemonEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event, "chat.chunk");
        assert_eq!(parsed.data["text"], "test");
    }
}
