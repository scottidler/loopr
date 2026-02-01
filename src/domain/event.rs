//! Event record types for observability.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{generate_signal_id, now_ms};

/// Event type constants
pub mod event_types {
    pub const LOOP_CREATED: &str = "loop.created";
    pub const LOOP_STARTED: &str = "loop.started";
    pub const LOOP_STATUS_CHANGE: &str = "loop.status_change";
    pub const ITERATION_STARTED: &str = "iteration.started";
    pub const ITERATION_COMPLETE: &str = "iteration.complete";
    pub const LOOP_COMPLETE: &str = "loop.complete";
    pub const LOOP_FAILED: &str = "loop.failed";
    pub const DAEMON_STARTED: &str = "daemon.started";
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
}

/// General-purpose event log for observability.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventRecord {
    /// Unique event identifier
    pub id: String,
    /// Event type (e.g., "loop.started", "iteration.complete")
    pub event_type: String,
    /// Associated loop ID (if any)
    pub loop_id: Option<String>,
    /// Event-specific payload data
    pub payload: Value,
    /// Unix timestamp in milliseconds
    pub created_at: i64,
}

impl EventRecord {
    /// Create a new event with the given type and payload
    pub fn new(event_type: &str, loop_id: Option<String>, payload: Value) -> Self {
        Self {
            id: generate_signal_id().replace("sig-", "evt-"),
            event_type: event_type.to_string(),
            loop_id,
            payload,
            created_at: now_ms(),
        }
    }

    /// Create a loop.created event
    pub fn loop_created(loop_id: &str, loop_type: &str) -> Self {
        Self::new(
            event_types::LOOP_CREATED,
            Some(loop_id.to_string()),
            serde_json::json!({ "loop_type": loop_type }),
        )
    }

    /// Create a loop.started event
    pub fn loop_started(loop_id: &str) -> Self {
        Self::new(
            event_types::LOOP_STARTED,
            Some(loop_id.to_string()),
            Value::Null,
        )
    }

    /// Create a loop.status_change event
    pub fn loop_status_change(loop_id: &str, old_status: &str, new_status: &str) -> Self {
        Self::new(
            event_types::LOOP_STATUS_CHANGE,
            Some(loop_id.to_string()),
            serde_json::json!({
                "old_status": old_status,
                "new_status": new_status
            }),
        )
    }

    /// Create an iteration.started event
    pub fn iteration_started(loop_id: &str, iteration: u32) -> Self {
        Self::new(
            event_types::ITERATION_STARTED,
            Some(loop_id.to_string()),
            serde_json::json!({ "iteration": iteration }),
        )
    }

    /// Create an iteration.complete event
    pub fn iteration_complete(loop_id: &str, iteration: u32, passed: bool, output: &str) -> Self {
        Self::new(
            event_types::ITERATION_COMPLETE,
            Some(loop_id.to_string()),
            serde_json::json!({
                "iteration": iteration,
                "passed": passed,
                "output": output
            }),
        )
    }

    /// Create a loop.complete event
    pub fn loop_complete(loop_id: &str, iterations: u32) -> Self {
        Self::new(
            event_types::LOOP_COMPLETE,
            Some(loop_id.to_string()),
            serde_json::json!({ "iterations": iterations }),
        )
    }

    /// Create a loop.failed event
    pub fn loop_failed(loop_id: &str, reason: &str) -> Self {
        Self::new(
            event_types::LOOP_FAILED,
            Some(loop_id.to_string()),
            serde_json::json!({ "reason": reason }),
        )
    }

    /// Create a daemon.started event
    pub fn daemon_started() -> Self {
        Self::new(event_types::DAEMON_STARTED, None, Value::Null)
    }

    /// Create a daemon.shutdown event
    pub fn daemon_shutdown(reason: &str) -> Self {
        Self::new(
            event_types::DAEMON_SHUTDOWN,
            None,
            serde_json::json!({ "reason": reason }),
        )
    }

    /// Check if this is a loop-related event
    pub fn is_loop_event(&self) -> bool {
        self.loop_id.is_some()
    }

    /// Check if this is a daemon-related event
    pub fn is_daemon_event(&self) -> bool {
        self.event_type.starts_with("daemon.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_record_new() {
        let event = EventRecord::new("test.event", Some("loop-123".to_string()), Value::Null);
        assert!(event.id.starts_with("evt-"));
        assert_eq!(event.event_type, "test.event");
        assert_eq!(event.loop_id, Some("loop-123".to_string()));
        assert!(event.created_at > 0);
    }

    #[test]
    fn test_loop_created() {
        let event = EventRecord::loop_created("loop-123", "plan");
        assert_eq!(event.event_type, event_types::LOOP_CREATED);
        assert_eq!(event.loop_id, Some("loop-123".to_string()));
        assert_eq!(event.payload["loop_type"], "plan");
    }

    #[test]
    fn test_loop_started() {
        let event = EventRecord::loop_started("loop-456");
        assert_eq!(event.event_type, event_types::LOOP_STARTED);
        assert_eq!(event.loop_id, Some("loop-456".to_string()));
    }

    #[test]
    fn test_loop_status_change() {
        let event = EventRecord::loop_status_change("loop-789", "pending", "running");
        assert_eq!(event.event_type, event_types::LOOP_STATUS_CHANGE);
        assert_eq!(event.payload["old_status"], "pending");
        assert_eq!(event.payload["new_status"], "running");
    }

    #[test]
    fn test_iteration_started() {
        let event = EventRecord::iteration_started("loop-123", 5);
        assert_eq!(event.event_type, event_types::ITERATION_STARTED);
        assert_eq!(event.payload["iteration"], 5);
    }

    #[test]
    fn test_iteration_complete() {
        let event = EventRecord::iteration_complete("loop-123", 3, true, "All tests passed");
        assert_eq!(event.event_type, event_types::ITERATION_COMPLETE);
        assert_eq!(event.payload["iteration"], 3);
        assert_eq!(event.payload["passed"], true);
        assert_eq!(event.payload["output"], "All tests passed");
    }

    #[test]
    fn test_loop_complete() {
        let event = EventRecord::loop_complete("loop-123", 7);
        assert_eq!(event.event_type, event_types::LOOP_COMPLETE);
        assert_eq!(event.payload["iterations"], 7);
    }

    #[test]
    fn test_loop_failed() {
        let event = EventRecord::loop_failed("loop-123", "Max iterations exceeded");
        assert_eq!(event.event_type, event_types::LOOP_FAILED);
        assert_eq!(event.payload["reason"], "Max iterations exceeded");
    }

    #[test]
    fn test_daemon_started() {
        let event = EventRecord::daemon_started();
        assert_eq!(event.event_type, event_types::DAEMON_STARTED);
        assert!(event.loop_id.is_none());
    }

    #[test]
    fn test_daemon_shutdown() {
        let event = EventRecord::daemon_shutdown("User requested");
        assert_eq!(event.event_type, event_types::DAEMON_SHUTDOWN);
        assert_eq!(event.payload["reason"], "User requested");
    }

    #[test]
    fn test_is_loop_event() {
        let loop_event = EventRecord::loop_started("loop-123");
        let daemon_event = EventRecord::daemon_started();
        assert!(loop_event.is_loop_event());
        assert!(!daemon_event.is_loop_event());
    }

    #[test]
    fn test_is_daemon_event() {
        let loop_event = EventRecord::loop_started("loop-123");
        let daemon_event = EventRecord::daemon_started();
        assert!(!loop_event.is_daemon_event());
        assert!(daemon_event.is_daemon_event());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let event = EventRecord::iteration_complete("loop-test", 2, false, "Test failed");
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: EventRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_event_types_constants() {
        assert_eq!(event_types::LOOP_CREATED, "loop.created");
        assert_eq!(event_types::LOOP_STARTED, "loop.started");
        assert_eq!(event_types::LOOP_STATUS_CHANGE, "loop.status_change");
        assert_eq!(event_types::ITERATION_STARTED, "iteration.started");
        assert_eq!(event_types::ITERATION_COMPLETE, "iteration.complete");
        assert_eq!(event_types::LOOP_COMPLETE, "loop.complete");
        assert_eq!(event_types::LOOP_FAILED, "loop.failed");
        assert_eq!(event_types::DAEMON_STARTED, "daemon.started");
        assert_eq!(event_types::DAEMON_SHUTDOWN, "daemon.shutdown");
    }
}
