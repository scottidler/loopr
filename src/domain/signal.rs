//! Signal types for inter-loop communication
//!
//! Signals provide coordination between loops: stop, pause, resume, invalidate.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{generate_signal_id, now_ms};

/// Type of signal for loop coordination
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalType {
    /// Terminate immediately
    Stop,
    /// Suspend execution (resumable)
    Pause,
    /// Continue paused loop
    Resume,
    /// Stop, rebase worktree, continue
    Rebase,
    /// Report problem upstream
    Error,
    /// Advisory message
    Info,
    /// Parent re-iterated, work is stale
    Invalidate,
}

/// A signal for loop-to-loop coordination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalRecord {
    /// Unique signal identifier
    pub id: String,
    /// Type of signal
    pub signal_type: SignalType,
    /// Loop that sent the signal (if any)
    pub source_loop: Option<String>,
    /// Specific loop to target (if any)
    pub target_loop: Option<String>,
    /// Selector for multiple targets (e.g., "descendants:001")
    pub target_selector: Option<String>,
    /// Human-readable reason for the signal
    pub reason: String,
    /// Additional data payload
    pub payload: Option<Value>,
    /// When the signal was created (Unix ms)
    pub created_at: i64,
    /// When the signal was acknowledged (Unix ms)
    pub acknowledged_at: Option<i64>,
}

impl SignalRecord {
    /// Create a new signal
    pub fn new(signal_type: SignalType, reason: impl Into<String>) -> Self {
        Self {
            id: generate_signal_id(),
            signal_type,
            source_loop: None,
            target_loop: None,
            target_selector: None,
            reason: reason.into(),
            payload: None,
            created_at: now_ms(),
            acknowledged_at: None,
        }
    }

    /// Set the source loop
    pub fn from_loop(mut self, loop_id: impl Into<String>) -> Self {
        self.source_loop = Some(loop_id.into());
        self
    }

    /// Set the target loop
    pub fn to_loop(mut self, loop_id: impl Into<String>) -> Self {
        self.target_loop = Some(loop_id.into());
        self
    }

    /// Set a target selector (e.g., "descendants:001")
    pub fn to_selector(mut self, selector: impl Into<String>) -> Self {
        self.target_selector = Some(selector.into());
        self
    }

    /// Add a payload
    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = Some(payload);
        self
    }

    /// Mark the signal as acknowledged
    pub fn acknowledge(&mut self) {
        self.acknowledged_at = Some(now_ms());
    }

    /// Check if the signal has been acknowledged
    pub fn is_acknowledged(&self) -> bool {
        self.acknowledged_at.is_some()
    }

    /// Check if this signal should stop the target loop
    pub fn is_stop_signal(&self) -> bool {
        matches!(
            self.signal_type,
            SignalType::Stop | SignalType::Invalidate
        )
    }

    /// Check if this signal pauses execution
    pub fn is_pause_signal(&self) -> bool {
        matches!(self.signal_type, SignalType::Pause | SignalType::Rebase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_signal_type_serialization() {
        assert_eq!(
            serde_json::to_string(&SignalType::Stop).unwrap(),
            "\"stop\""
        );
        assert_eq!(
            serde_json::to_string(&SignalType::Pause).unwrap(),
            "\"pause\""
        );
        assert_eq!(
            serde_json::to_string(&SignalType::Invalidate).unwrap(),
            "\"invalidate\""
        );
    }

    #[test]
    fn test_signal_type_deserialization() {
        assert_eq!(
            serde_json::from_str::<SignalType>("\"stop\"").unwrap(),
            SignalType::Stop
        );
        assert_eq!(
            serde_json::from_str::<SignalType>("\"resume\"").unwrap(),
            SignalType::Resume
        );
    }

    #[test]
    fn test_new_signal() {
        let signal = SignalRecord::new(SignalType::Stop, "User requested stop");
        assert!(signal.id.starts_with("sig-"));
        assert_eq!(signal.signal_type, SignalType::Stop);
        assert_eq!(signal.reason, "User requested stop");
        assert!(signal.source_loop.is_none());
        assert!(signal.target_loop.is_none());
        assert!(signal.acknowledged_at.is_none());
    }

    #[test]
    fn test_signal_builder_from_loop() {
        let signal = SignalRecord::new(SignalType::Pause, "Pausing")
            .from_loop("parent-001");
        assert_eq!(signal.source_loop, Some("parent-001".to_string()));
    }

    #[test]
    fn test_signal_builder_to_loop() {
        let signal = SignalRecord::new(SignalType::Stop, "Stopping")
            .to_loop("child-001-002");
        assert_eq!(signal.target_loop, Some("child-001-002".to_string()));
    }

    #[test]
    fn test_signal_builder_to_selector() {
        let signal = SignalRecord::new(SignalType::Invalidate, "Parent changed")
            .to_selector("descendants:001");
        assert_eq!(signal.target_selector, Some("descendants:001".to_string()));
    }

    #[test]
    fn test_signal_builder_with_payload() {
        let signal = SignalRecord::new(SignalType::Error, "Something failed")
            .with_payload(json!({"error_code": 42}));
        assert!(signal.payload.is_some());
        assert_eq!(signal.payload.unwrap()["error_code"], 42);
    }

    #[test]
    fn test_signal_acknowledge() {
        let mut signal = SignalRecord::new(SignalType::Stop, "Stop");
        assert!(!signal.is_acknowledged());
        signal.acknowledge();
        assert!(signal.is_acknowledged());
        assert!(signal.acknowledged_at.is_some());
    }

    #[test]
    fn test_is_stop_signal() {
        assert!(SignalRecord::new(SignalType::Stop, "").is_stop_signal());
        assert!(SignalRecord::new(SignalType::Invalidate, "").is_stop_signal());
        assert!(!SignalRecord::new(SignalType::Pause, "").is_stop_signal());
        assert!(!SignalRecord::new(SignalType::Resume, "").is_stop_signal());
    }

    #[test]
    fn test_is_pause_signal() {
        assert!(SignalRecord::new(SignalType::Pause, "").is_pause_signal());
        assert!(SignalRecord::new(SignalType::Rebase, "").is_pause_signal());
        assert!(!SignalRecord::new(SignalType::Stop, "").is_pause_signal());
        assert!(!SignalRecord::new(SignalType::Resume, "").is_pause_signal());
    }

    #[test]
    fn test_signal_serialization_roundtrip() {
        let signal = SignalRecord::new(SignalType::Invalidate, "Parent re-iterated")
            .from_loop("001")
            .to_selector("descendants:001")
            .with_payload(json!({"old_iteration": 3}));

        let json = serde_json::to_string(&signal).unwrap();
        let deserialized: SignalRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, signal.id);
        assert_eq!(deserialized.signal_type, SignalType::Invalidate);
        assert_eq!(deserialized.source_loop, Some("001".to_string()));
        assert_eq!(deserialized.target_selector, Some("descendants:001".to_string()));
    }
}
