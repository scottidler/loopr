use serde::{Deserialize, Serialize};

use crate::agents::status::AgentStatus;

/// Events emitted by agents, broadcast through the daemon event system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    StatusChange {
        session_id: String,
        status: AgentStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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
}
