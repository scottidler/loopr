use serde::{Deserialize, Serialize};

use crate::agents::reviewer::ReviewVerdict;
use crate::agents::status::AgentStatus;

/// Events emitted by agents, broadcast through the daemon event system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", rename_all_fields = "kebab-case")]
pub enum AgentEvent {
    StatusChange {
        session_id: String,
        status: AgentStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// A reviewer session reached a final verdict on its bundle. Carries both
    /// the bundle and its parent work so event-trigger scope extraction can
    /// route `reviewer-approved`/`reviewer-rejected` (bundle-scoped) and
    /// `reviewer-rejected-work` (work-scoped) off the same event.
    ReviewerVerdict {
        session_id: String,
        bundle_id: String,
        work_id: String,
        verdict: ReviewVerdict,
    },
    LlmOutput {
        session_id: String,
        chunk: String,
        is_final: bool,
    },
    ToolStarted {
        session_id: String,
        tool: String,
    },
    ToolCompleted {
        session_id: String,
        tool: String,
        exit_code: i32,
        duration_ms: u64,
    },
    ActionCompleted {
        session_id: String,
        action_summary: String,
    },
    IterationCompleted {
        session_id: String,
        iteration: u32,
        summary: String,
    },
    StalenessDetected {
        session_id: String,
        new_tick_id: String,
    },
    TimingInfo {
        session_id: String,
        label: String,
        detail: String,
    },
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_event_status_change_serde() {
        let event = AgentEvent::StatusChange {
            session_id: "s1".to_string(),
            status: AgentStatus::Running,
            error: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("error"), "error field should be skipped when None");
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::StatusChange {
            session_id,
            status,
            error,
        } = deserialized
        {
            assert_eq!(session_id, "s1");
            assert_eq!(status, AgentStatus::Running);
            assert!(error.is_none());
        } else {
            panic!("expected StatusChange");
        }
    }

    #[test]
    fn test_agent_event_status_change_with_error_serde() {
        let event = AgentEvent::StatusChange {
            session_id: "s2".to_string(),
            status: AgentStatus::Failed,
            error: Some("API key not found".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("API key not found"));
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::StatusChange {
            session_id,
            status,
            error,
        } = deserialized
        {
            assert_eq!(session_id, "s2");
            assert_eq!(status, AgentStatus::Failed);
            assert_eq!(error.as_deref(), Some("API key not found"));
        } else {
            panic!("expected StatusChange");
        }
    }

    #[test]
    fn test_agent_event_llm_output_serde() {
        let event = AgentEvent::LlmOutput {
            session_id: "s1".to_string(),
            chunk: "Hello".to_string(),
            is_final: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::LlmOutput {
            session_id,
            chunk,
            is_final,
        } = deserialized
        {
            assert_eq!(session_id, "s1");
            assert_eq!(chunk, "Hello");
            assert!(!is_final);
        } else {
            panic!("expected LlmOutput");
        }
    }

    #[test]
    fn test_agent_event_tool_completed_serde() {
        let event = AgentEvent::ToolCompleted {
            session_id: "s1".to_string(),
            tool: "test".to_string(),
            exit_code: 0,
            duration_ms: 1500,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::ToolCompleted {
            session_id,
            tool,
            exit_code,
            duration_ms,
        } = deserialized
        {
            assert_eq!(session_id, "s1");
            assert_eq!(tool, "test");
            assert_eq!(exit_code, 0);
            assert_eq!(duration_ms, 1500);
        } else {
            panic!("expected ToolCompleted");
        }
    }

    #[test]
    fn test_agent_event_staleness_detected_serde() {
        let event = AgentEvent::StalenessDetected {
            session_id: "s1".to_string(),
            new_tick_id: "tick-42".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::StalenessDetected {
            session_id,
            new_tick_id,
        } = deserialized
        {
            assert_eq!(session_id, "s1");
            assert_eq!(new_tick_id, "tick-42");
        } else {
            panic!("expected StalenessDetected");
        }
    }

    #[test]
    fn reviewer_verdict_serde_uses_kebab_case_keys() {
        let event = AgentEvent::ReviewerVerdict {
            session_id: "rev-session".to_string(),
            bundle_id: "bd-1".to_string(),
            work_id: "wk-1".to_string(),
            verdict: ReviewVerdict::Reject,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"bundle-id\""), "expected kebab bundle-id in {json}");
        assert!(json.contains("\"work-id\""), "expected kebab work-id in {json}");
        assert!(json.contains("\"session-id\""), "expected kebab session-id in {json}");
        assert!(json.contains("\"reject\""), "expected verdict=reject in {json}");
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            AgentEvent::ReviewerVerdict {
                session_id,
                bundle_id,
                work_id,
                verdict,
            } => {
                assert_eq!(session_id, "rev-session");
                assert_eq!(bundle_id, "bd-1");
                assert_eq!(work_id, "wk-1");
                assert_eq!(verdict, ReviewVerdict::Reject);
            }
            other => panic!("expected ReviewerVerdict, got {other:?}"),
        }
    }
}
