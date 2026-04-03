use std::collections::HashMap;

use eyre::Result;
use serde::{Deserialize, Serialize};
use taskstore::record::{IndexValue, Record};

use crate::agents::kind::AgentKind;
use crate::agents::status::AgentStatus;
use crate::id;

/// A persistent record tracking an agent's lifecycle and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub agent_type: AgentKind,
    pub work_id: Option<String>,
    pub bundle_id: Option<String>,
    status: AgentStatus,
    pub iteration: u32,
    pub model: String,
    pub worktree_path: Option<String>,
    pub error_message: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Generic target ID for agents that don't target Works or Bundles.
    /// Coordinator: None (operates globally).
    /// Researcher: the scope_id (Plan/Spec/Phase/Work ID being researched).
    /// Integrator: None (operates on whatever Accepted Bundles exist).
    #[serde(default)]
    pub target_id: Option<String>,
    /// Query string for Researcher agents. Set by SpawnResearcher action.
    #[serde(default)]
    pub query: Option<String>,
    /// Daemon session ID that spawned this agent (e.g. "20260305T143200").
    #[serde(default)]
    pub daemon_session_id: Option<String>,
    /// Classified error kind for Coordinator dispatch (set on failure).
    #[serde(default)]
    pub error_kind: Option<crate::agents::error::AgentErrorKind>,
}

impl AgentSession {
    pub fn new(agent_type: AgentKind, model: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id("ag"),
            agent_type,
            work_id: None,
            bundle_id: None,
            status: AgentStatus::Starting,
            iteration: 0,
            model,
            worktree_path: None,
            error_message: None,
            created_at: now,
            updated_at: now,
            target_id: None,
            query: None,
            daemon_session_id: None,
            error_kind: None,
        }
    }

    /// Read current status.
    pub fn status(&self) -> AgentStatus {
        self.status
    }

    /// Bypass FSM validation. For recovery, bootstrap, and test fixtures ONLY.
    pub fn force_status(&mut self, target: AgentStatus) {
        self.status = target;
        self.updated_at = id::now_millis();
    }

    /// Transition the agent to a new status, updating the timestamp.
    /// Returns Err if the transition is not allowed.
    pub fn transition_to(&mut self, target: AgentStatus) -> Result<(), String> {
        if !self.status.can_transition_to(target) {
            return Err(format!(
                "invalid agent status transition: {} \u{2192} {}",
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
        m.insert("agent_type".into(), IndexValue::String(self.agent_type.to_string()));
        if let Some(ref wi_id) = self.work_id {
            m.insert("work_id".into(), IndexValue::String(wi_id.clone()));
        }
        if let Some(ref b_id) = self.bundle_id {
            m.insert("bundle_id".into(), IndexValue::String(b_id.clone()));
        }
        if let Some(ref tid) = self.target_id {
            m.insert("target_id".into(), IndexValue::String(tid.clone()));
        }
        if let Some(ref dsid) = self.daemon_session_id {
            m.insert("daemon_session_id".into(), IndexValue::String(dsid.clone()));
        }
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use taskstore::record::Record;

    #[test]
    fn test_agent_session_new() {
        let session = AgentSession::new(AgentKind::Implementer, "claude-sonnet-4-6".to_string());
        assert!(!session.id.is_empty());
        assert_eq!(session.agent_type, AgentKind::Implementer);
        assert_eq!(session.status(), AgentStatus::Starting);
        assert_eq!(session.iteration, 0);
        assert_eq!(session.model, "claude-sonnet-4-6");
        assert!(session.work_id.is_none());
        assert!(session.bundle_id.is_none());
        assert!(session.worktree_path.is_none());
        assert!(session.error_message.is_none());
        assert!(session.target_id.is_none());
        assert!(session.query.is_none());
        assert!(session.created_at > 0);
        assert_eq!(session.created_at, session.updated_at);
    }

    #[test]
    fn test_agent_session_new_researcher_with_target() {
        let mut session = AgentSession::new(AgentKind::Researcher, "model".to_string());
        session.target_id = Some("wi-123".to_string());
        session.query = Some("Investigate auth module".to_string());
        assert_eq!(session.agent_type, AgentKind::Researcher);
        assert_eq!(session.target_id.as_deref(), Some("wi-123"));
        assert_eq!(session.query.as_deref(), Some("Investigate auth module"));
    }

    #[test]
    fn test_agent_session_new_coordinator() {
        let session = AgentSession::new(AgentKind::Coordinator, "model".to_string());
        assert_eq!(session.agent_type, AgentKind::Coordinator);
        assert!(session.target_id.is_none());
        assert!(session.query.is_none());
    }

    #[test]
    fn test_agent_session_unique_ids() {
        let s1 = AgentSession::new(AgentKind::Implementer, "m".to_string());
        let s2 = AgentSession::new(AgentKind::Implementer, "m".to_string());
        assert_ne!(s1.id, s2.id);
    }

    #[test]
    fn test_agent_session_transition_valid() {
        let mut session = AgentSession::new(AgentKind::Implementer, "m".to_string());
        assert!(session.transition_to(AgentStatus::Running).is_ok());
        assert_eq!(session.status(), AgentStatus::Running);
        assert!(session.updated_at >= session.created_at);
    }

    #[test]
    fn test_agent_session_transition_invalid() {
        let mut session = AgentSession::new(AgentKind::Implementer, "m".to_string());
        let result = session.transition_to(AgentStatus::Completed);
        assert!(result.is_err());
        assert_eq!(session.status(), AgentStatus::Starting); // unchanged
    }

    #[test]
    fn test_agent_session_transition_chain() {
        let mut session = AgentSession::new(AgentKind::Reviewer, "m".to_string());
        assert!(session.transition_to(AgentStatus::Running).is_ok());
        assert!(session.transition_to(AgentStatus::WaitingForLlm).is_ok());
        assert!(session.transition_to(AgentStatus::Running).is_ok());
        assert!(session.transition_to(AgentStatus::Completed).is_ok());
        assert!(session.status().is_terminal());
    }

    #[test]
    fn test_agent_session_serde_roundtrip() {
        let mut session = AgentSession::new(AgentKind::Implementer, "claude-sonnet-4-6".to_string());
        session.work_id = Some("wi-123".to_string());
        session.worktree_path = Some("/tmp/worktree".to_string());
        let json = serde_json::to_string(&session).unwrap();
        let deserialized: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session.id, deserialized.id);
        assert_eq!(session.agent_type, deserialized.agent_type);
        assert_eq!(session.status(), deserialized.status());
        assert_eq!(session.work_id, deserialized.work_id);
        assert_eq!(session.worktree_path, deserialized.worktree_path);
        assert_eq!(session.target_id, deserialized.target_id);
        assert_eq!(session.query, deserialized.query);
    }

    #[test]
    fn test_agent_session_serde_backward_compat() {
        // Old JSON without target_id/query should deserialize with defaults (None)
        let json = r#"{
            "id": "test-1", "agent_type": "implementer", "work_id": null,
            "bundle_id": null, "status": "starting", "iteration": 0,
            "model": "m", "worktree_path": null, "error_message": null,
            "created_at": 1000, "updated_at": 1000
        }"#;
        let session: AgentSession = serde_json::from_str(json).unwrap();
        assert!(session.target_id.is_none());
        assert!(session.query.is_none());
    }

    #[test]
    fn test_agent_session_record_id() {
        let session = AgentSession::new(AgentKind::Implementer, "m".to_string());
        assert_eq!(Record::id(&session), session.id.as_str());
    }

    #[test]
    fn test_agent_session_record_updated_at() {
        let session = AgentSession::new(AgentKind::Implementer, "m".to_string());
        assert_eq!(Record::updated_at(&session), session.updated_at);
    }

    #[test]
    fn test_agent_session_record_collection_name() {
        assert_eq!(AgentSession::collection_name(), "agent_sessions");
    }

    #[test]
    fn test_agent_session_record_indexed_fields() {
        let mut session = AgentSession::new(AgentKind::Implementer, "m".to_string());
        session.work_id = Some("wi-1".to_string());

        let fields = session.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("starting".to_string())));
        assert_eq!(
            fields.get("agent_type"),
            Some(&IndexValue::String("implementer".to_string()))
        );
        assert_eq!(fields.get("work_id"), Some(&IndexValue::String("wi-1".to_string())));
        assert!(!fields.contains_key("bundle_id"));
    }

    #[test]
    fn test_agent_session_record_indexed_fields_reviewer() {
        let mut session = AgentSession::new(AgentKind::Reviewer, "m".to_string());
        session.bundle_id = Some("b-1".to_string());

        let fields = session.indexed_fields();
        assert_eq!(
            fields.get("agent_type"),
            Some(&IndexValue::String("reviewer".to_string()))
        );
        assert_eq!(fields.get("bundle_id"), Some(&IndexValue::String("b-1".to_string())));
        assert!(!fields.contains_key("work_id"));
    }

    #[test]
    fn test_agent_session_record_indexed_fields_with_target_id() {
        let mut session = AgentSession::new(AgentKind::Researcher, "m".to_string());
        session.target_id = Some("wi-42".to_string());

        let fields = session.indexed_fields();
        assert_eq!(
            fields.get("agent_type"),
            Some(&IndexValue::String("researcher".to_string()))
        );
        assert_eq!(fields.get("target_id"), Some(&IndexValue::String("wi-42".to_string())));
    }
}
