pub mod bridge;
pub mod executor;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use taskstore::record::{IndexValue, Record};

use crate::id;

/// The type of agent — determines behavior and prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    Implementer,
    Reviewer,
}

impl fmt::Display for AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentType::Implementer => write!(f, "implementer"),
            AgentType::Reviewer => write!(f, "reviewer"),
        }
    }
}

/// Lifecycle status of an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Starting,
    Running,
    WaitingForLlm,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl AgentStatus {
    /// Returns true if this is a terminal status (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(self, AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Cancelled)
    }

    /// Validate whether a status transition is allowed.
    pub fn can_transition_to(&self, target: AgentStatus) -> bool {
        matches!(
            (self, target),
            // Starting transitions
            (AgentStatus::Starting, AgentStatus::Running)
            | (AgentStatus::Starting, AgentStatus::Failed)
            | (AgentStatus::Starting, AgentStatus::Cancelled)
            // Running transitions
            | (AgentStatus::Running, AgentStatus::WaitingForLlm)
            | (AgentStatus::Running, AgentStatus::Paused)
            | (AgentStatus::Running, AgentStatus::Completed)
            | (AgentStatus::Running, AgentStatus::Failed)
            | (AgentStatus::Running, AgentStatus::Cancelled)
            // WaitingForLlm transitions
            | (AgentStatus::WaitingForLlm, AgentStatus::Running)
            | (AgentStatus::WaitingForLlm, AgentStatus::Failed)
            | (AgentStatus::WaitingForLlm, AgentStatus::Cancelled)
            // Paused transitions
            | (AgentStatus::Paused, AgentStatus::Running)
            | (AgentStatus::Paused, AgentStatus::Cancelled)
        )
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentStatus::Starting => write!(f, "starting"),
            AgentStatus::Running => write!(f, "running"),
            AgentStatus::WaitingForLlm => write!(f, "waitingforllm"),
            AgentStatus::Paused => write!(f, "paused"),
            AgentStatus::Completed => write!(f, "completed"),
            AgentStatus::Failed => write!(f, "failed"),
            AgentStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A persistent record tracking an agent's lifecycle and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub agent_type: AgentType,
    pub work_item_id: Option<String>,
    pub bundle_id: Option<String>,
    pub status: AgentStatus,
    pub iteration: u32,
    pub model: String,
    pub worktree_path: Option<String>,
    pub error_message: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl AgentSession {
    pub fn new(agent_type: AgentType, model: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            agent_type,
            work_item_id: None,
            bundle_id: None,
            status: AgentStatus::Starting,
            iteration: 0,
            model,
            worktree_path: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Transition the agent to a new status, updating the timestamp.
    /// Returns Err if the transition is not allowed.
    pub fn transition_to(&mut self, target: AgentStatus) -> Result<(), String> {
        if !self.status.can_transition_to(target) {
            return Err(format!(
                "invalid agent status transition: {} → {}",
                self.status, target
            ));
        }
        self.status = target;
        self.updated_at = id::now_millis();
        Ok(())
    }
}

impl Record for AgentSession {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "agent_sessions"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m.insert(
            "agent_type".into(),
            IndexValue::String(self.agent_type.to_string()),
        );
        if let Some(ref wi_id) = self.work_item_id {
            m.insert("work_item_id".into(), IndexValue::String(wi_id.clone()));
        }
        if let Some(ref b_id) = self.bundle_id {
            m.insert("bundle_id".into(), IndexValue::String(b_id.clone()));
        }
        m
    }
}

/// Structured actions that an LLM agent can request.
/// The agent's response is parsed into a sequence of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AgentAction {
    RunTool {
        tool_name: String,
        #[serde(default)]
        args: Vec<String>,
    },
    WriteFile {
        path: String,
        content: String,
    },
    ReadFile {
        path: String,
    },
    Commit {
        message: String,
        #[serde(default)]
        paths: Vec<String>,
    },
    ProposeBundle {
        description: String,
        #[serde(default)]
        claims: Vec<String>,
    },
    Transition {
        collection: String,
        id: String,
        target_state: String,
    },
    CreateLearning {
        content: String,
        scope: String,
        source_id: String,
    },
    Done {
        summary: String,
    },
    NeedHelp {
        reason: String,
    },
}

/// Events emitted by agents, broadcast through the daemon event system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    StatusChange {
        session_id: String,
        status: AgentStatus,
    },
    LlmOutput {
        session_id: String,
        chunk: String,
        is_final: bool,
    },
    ToolStarted {
        session_id: String,
        tool_name: String,
    },
    ToolCompleted {
        session_id: String,
        tool_name: String,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- AgentType tests ---

    #[test]
    fn test_agent_type_display() {
        assert_eq!(AgentType::Implementer.to_string(), "implementer");
        assert_eq!(AgentType::Reviewer.to_string(), "reviewer");
    }

    #[test]
    fn test_agent_type_serde_roundtrip() {
        for at in [AgentType::Implementer, AgentType::Reviewer] {
            let json = serde_json::to_string(&at).unwrap();
            let deserialized: AgentType = serde_json::from_str(&json).unwrap();
            assert_eq!(at, deserialized);
        }
    }

    #[test]
    fn test_agent_type_display_matches_serde() {
        for at in [AgentType::Implementer, AgentType::Reviewer] {
            let display = at.to_string();
            let quoted = format!("\"{display}\"");
            let deserialized: AgentType = serde_json::from_str(&quoted).unwrap();
            assert_eq!(at, deserialized);
        }
    }

    // --- AgentStatus tests ---

    #[test]
    fn test_agent_status_display() {
        assert_eq!(AgentStatus::Starting.to_string(), "starting");
        assert_eq!(AgentStatus::Running.to_string(), "running");
        assert_eq!(AgentStatus::WaitingForLlm.to_string(), "waitingforllm");
        assert_eq!(AgentStatus::Paused.to_string(), "paused");
        assert_eq!(AgentStatus::Completed.to_string(), "completed");
        assert_eq!(AgentStatus::Failed.to_string(), "failed");
        assert_eq!(AgentStatus::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn test_agent_status_serde_roundtrip() {
        for status in [
            AgentStatus::Starting,
            AgentStatus::Running,
            AgentStatus::WaitingForLlm,
            AgentStatus::Paused,
            AgentStatus::Completed,
            AgentStatus::Failed,
            AgentStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: AgentStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_agent_status_is_terminal() {
        assert!(!AgentStatus::Starting.is_terminal());
        assert!(!AgentStatus::Running.is_terminal());
        assert!(!AgentStatus::WaitingForLlm.is_terminal());
        assert!(!AgentStatus::Paused.is_terminal());
        assert!(AgentStatus::Completed.is_terminal());
        assert!(AgentStatus::Failed.is_terminal());
        assert!(AgentStatus::Cancelled.is_terminal());
    }

    #[test]
    fn test_agent_status_valid_transitions() {
        // Starting transitions
        assert!(AgentStatus::Starting.can_transition_to(AgentStatus::Running));
        assert!(AgentStatus::Starting.can_transition_to(AgentStatus::Failed));
        assert!(AgentStatus::Starting.can_transition_to(AgentStatus::Cancelled));

        // Running transitions
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::WaitingForLlm));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Paused));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Completed));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Failed));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Cancelled));

        // WaitingForLlm transitions
        assert!(AgentStatus::WaitingForLlm.can_transition_to(AgentStatus::Running));
        assert!(AgentStatus::WaitingForLlm.can_transition_to(AgentStatus::Failed));
        assert!(AgentStatus::WaitingForLlm.can_transition_to(AgentStatus::Cancelled));

        // Paused transitions
        assert!(AgentStatus::Paused.can_transition_to(AgentStatus::Running));
        assert!(AgentStatus::Paused.can_transition_to(AgentStatus::Cancelled));
    }

    #[test]
    fn test_agent_status_invalid_transitions() {
        // Terminal states cannot transition
        assert!(!AgentStatus::Completed.can_transition_to(AgentStatus::Running));
        assert!(!AgentStatus::Failed.can_transition_to(AgentStatus::Running));
        assert!(!AgentStatus::Cancelled.can_transition_to(AgentStatus::Running));

        // Cannot skip states
        assert!(!AgentStatus::Starting.can_transition_to(AgentStatus::Completed));
        assert!(!AgentStatus::Starting.can_transition_to(AgentStatus::Paused));
        assert!(!AgentStatus::Paused.can_transition_to(AgentStatus::Completed));
        assert!(!AgentStatus::Paused.can_transition_to(AgentStatus::WaitingForLlm));
    }

    // --- AgentSession tests ---

    #[test]
    fn test_agent_session_new() {
        let session = AgentSession::new(AgentType::Implementer, "claude-sonnet-4-6".to_string());
        assert!(!session.id.is_empty());
        assert_eq!(session.agent_type, AgentType::Implementer);
        assert_eq!(session.status, AgentStatus::Starting);
        assert_eq!(session.iteration, 0);
        assert_eq!(session.model, "claude-sonnet-4-6");
        assert!(session.work_item_id.is_none());
        assert!(session.bundle_id.is_none());
        assert!(session.worktree_path.is_none());
        assert!(session.error_message.is_none());
        assert!(session.created_at > 0);
        assert_eq!(session.created_at, session.updated_at);
    }

    #[test]
    fn test_agent_session_unique_ids() {
        let s1 = AgentSession::new(AgentType::Implementer, "m".to_string());
        let s2 = AgentSession::new(AgentType::Implementer, "m".to_string());
        assert_ne!(s1.id, s2.id);
    }

    #[test]
    fn test_agent_session_transition_valid() {
        let mut session = AgentSession::new(AgentType::Implementer, "m".to_string());
        assert!(session.transition_to(AgentStatus::Running).is_ok());
        assert_eq!(session.status, AgentStatus::Running);
        assert!(session.updated_at >= session.created_at);
    }

    #[test]
    fn test_agent_session_transition_invalid() {
        let mut session = AgentSession::new(AgentType::Implementer, "m".to_string());
        let result = session.transition_to(AgentStatus::Completed);
        assert!(result.is_err());
        assert_eq!(session.status, AgentStatus::Starting); // unchanged
    }

    #[test]
    fn test_agent_session_transition_chain() {
        let mut session = AgentSession::new(AgentType::Reviewer, "m".to_string());
        assert!(session.transition_to(AgentStatus::Running).is_ok());
        assert!(session.transition_to(AgentStatus::WaitingForLlm).is_ok());
        assert!(session.transition_to(AgentStatus::Running).is_ok());
        assert!(session.transition_to(AgentStatus::Completed).is_ok());
        assert!(session.status.is_terminal());
    }

    #[test]
    fn test_agent_session_serde_roundtrip() {
        let mut session = AgentSession::new(AgentType::Implementer, "claude-sonnet-4-6".to_string());
        session.work_item_id = Some("wi-123".to_string());
        session.worktree_path = Some("/tmp/worktree".to_string());
        let json = serde_json::to_string(&session).unwrap();
        let deserialized: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session.id, deserialized.id);
        assert_eq!(session.agent_type, deserialized.agent_type);
        assert_eq!(session.status, deserialized.status);
        assert_eq!(session.work_item_id, deserialized.work_item_id);
        assert_eq!(session.worktree_path, deserialized.worktree_path);
    }

    // --- Record trait tests ---

    #[test]
    fn test_agent_session_record_id() {
        let session = AgentSession::new(AgentType::Implementer, "m".to_string());
        assert_eq!(Record::id(&session), session.id.as_str());
    }

    #[test]
    fn test_agent_session_record_updated_at() {
        let session = AgentSession::new(AgentType::Implementer, "m".to_string());
        assert_eq!(Record::updated_at(&session), session.updated_at);
    }

    #[test]
    fn test_agent_session_record_collection_name() {
        assert_eq!(AgentSession::collection_name(), "agent_sessions");
    }

    #[test]
    fn test_agent_session_record_indexed_fields() {
        let mut session = AgentSession::new(AgentType::Implementer, "m".to_string());
        session.work_item_id = Some("wi-1".to_string());

        let fields = session.indexed_fields();
        assert_eq!(
            fields.get("status"),
            Some(&IndexValue::String("starting".to_string()))
        );
        assert_eq!(
            fields.get("agent_type"),
            Some(&IndexValue::String("implementer".to_string()))
        );
        assert_eq!(
            fields.get("work_item_id"),
            Some(&IndexValue::String("wi-1".to_string()))
        );
        assert!(!fields.contains_key("bundle_id"));
    }

    #[test]
    fn test_agent_session_record_indexed_fields_reviewer() {
        let mut session = AgentSession::new(AgentType::Reviewer, "m".to_string());
        session.bundle_id = Some("b-1".to_string());

        let fields = session.indexed_fields();
        assert_eq!(
            fields.get("agent_type"),
            Some(&IndexValue::String("reviewer".to_string()))
        );
        assert_eq!(
            fields.get("bundle_id"),
            Some(&IndexValue::String("b-1".to_string()))
        );
        assert!(!fields.contains_key("work_item_id"));
    }

    // --- AgentAction tests ---

    #[test]
    fn test_agent_action_run_tool_serde() {
        let action = AgentAction::RunTool {
            tool_name: "test".to_string(),
            args: vec![],
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::RunTool { tool_name, args } = deserialized {
            assert_eq!(tool_name, "test");
            assert!(args.is_empty());
        } else {
            panic!("expected RunTool");
        }
    }

    #[test]
    fn test_agent_action_write_file_serde() {
        let action = AgentAction::WriteFile {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::WriteFile { path, content } = deserialized {
            assert_eq!(path, "src/main.rs");
            assert_eq!(content, "fn main() {}");
        } else {
            panic!("expected WriteFile");
        }
    }

    #[test]
    fn test_agent_action_done_serde() {
        let action = AgentAction::Done {
            summary: "All tests pass".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::Done { summary } = deserialized {
            assert_eq!(summary, "All tests pass");
        } else {
            panic!("expected Done");
        }
    }

    #[test]
    fn test_agent_action_parse_from_llm_json() {
        let llm_output = r#"[
            {"action": "write_file", "path": "src/foo.rs", "content": "pub fn foo() {}"},
            {"action": "run_tool", "tool_name": "test", "args": []},
            {"action": "commit", "message": "feat: add foo", "paths": ["src/foo.rs"]},
            {"action": "done", "summary": "Implemented foo"}
        ]"#;
        let actions: Vec<AgentAction> = serde_json::from_str(llm_output).unwrap();
        assert_eq!(actions.len(), 4);
        assert!(matches!(actions[0], AgentAction::WriteFile { .. }));
        assert!(matches!(actions[1], AgentAction::RunTool { .. }));
        assert!(matches!(actions[2], AgentAction::Commit { .. }));
        assert!(matches!(actions[3], AgentAction::Done { .. }));
    }

    #[test]
    fn test_agent_action_need_help_serde() {
        let action = AgentAction::NeedHelp {
            reason: "Ambiguous requirement".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::NeedHelp { reason } = deserialized {
            assert_eq!(reason, "Ambiguous requirement");
        } else {
            panic!("expected NeedHelp");
        }
    }

    #[test]
    fn test_agent_action_propose_bundle_serde() {
        let action = AgentAction::ProposeBundle {
            description: "Add error handling".to_string(),
            claims: vec!["src/error.rs".to_string()],
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::ProposeBundle { description, claims } = deserialized {
            assert_eq!(description, "Add error handling");
            assert_eq!(claims, vec!["src/error.rs"]);
        } else {
            panic!("expected ProposeBundle");
        }
    }

    #[test]
    fn test_agent_action_create_learning_serde() {
        let action = AgentAction::CreateLearning {
            content: "Parser needs error recovery".to_string(),
            scope: "work_item".to_string(),
            source_id: "wi-1".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::CreateLearning {
            content,
            scope,
            source_id,
        } = deserialized
        {
            assert_eq!(content, "Parser needs error recovery");
            assert_eq!(scope, "work_item");
            assert_eq!(source_id, "wi-1");
        } else {
            panic!("expected CreateLearning");
        }
    }

    // --- AgentEvent tests ---

    #[test]
    fn test_agent_event_status_change_serde() {
        let event = AgentEvent::StatusChange {
            session_id: "s1".to_string(),
            status: AgentStatus::Running,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::StatusChange { session_id, status } = deserialized {
            assert_eq!(session_id, "s1");
            assert_eq!(status, AgentStatus::Running);
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
            tool_name: "test".to_string(),
            exit_code: 0,
            duration_ms: 1500,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::ToolCompleted {
            session_id,
            tool_name,
            exit_code,
            duration_ms,
        } = deserialized
        {
            assert_eq!(session_id, "s1");
            assert_eq!(tool_name, "test");
            assert_eq!(exit_code, 0);
            assert_eq!(duration_ms, 1500);
        } else {
            panic!("expected ToolCompleted");
        }
    }
}
