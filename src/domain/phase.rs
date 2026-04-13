use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use taskstore::record::{IndexValue, Record};

use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::markdown::{DocMarkdown, FmValue, millis_to_iso};
use crate::domain::plan::HierarchyStatus;
use crate::id;
use crate::prompts::SECTION_AC;

/// Type alias so Phase can name its own status type.
pub type PhaseStatus = HierarchyStatus;

/// Implementation phase within a Spec. Ordered by `dependencies` (linked list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    status: PhaseStatus,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub activated_at: Option<i64>,
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    /// Number of decomposition attempts on this phase (for phase-decomposition-attempt-limit trigger).
    #[serde(default)]
    pub decomposition_attempts: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Phase {
    /// Read current status.
    pub fn status(&self) -> PhaseStatus {
        self.status
    }

    /// Validated FSM transition via the runtime interpreter.
    pub fn transition(
        &mut self,
        target: PhaseStatus,
        role: crate::domain::role::Role,
        fsm: &crate::fsm::runtime::FsmInterpreter,
    ) -> eyre::Result<crate::domain::transition::Transition> {
        use crate::fsm::status::FsmStatus;
        let result = fsm.validate_transition(
            crate::domain::plan::HierarchyStatus::fsm_name(),
            self.status.to_yaml_name(),
            target.to_yaml_name(),
            &role.to_string(),
        )?;
        if result == crate::domain::transition::Transition::Changed {
            self.status = target;
            self.updated_at = crate::id::now_millis();
        }
        Ok(result)
    }

    /// Bypass FSM validation. For recovery, bootstrap, and test fixtures ONLY.
    pub fn force_status(&mut self, target: PhaseStatus) {
        self.status = target;
        self.updated_at = crate::id::now_millis();
    }

    pub fn new(parent_id: String, title: String) -> Self {
        tracing::debug!("Phase::new(parent_id={}, title={})", parent_id, title);
        let now = id::now_millis();
        Self {
            id: id::generate_id("ph"),
            parent_id,
            title,
            status: PhaseStatus::Draft,
            dependencies: Vec::new(),
            activated_at: None,
            acceptance_criteria: AcceptanceCriteria::default(),
            decomposition_attempts: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

impl DocMarkdown for Phase {
    fn doc_id(&self) -> &str {
        &self.id
    }

    fn doc_body(&self) -> String {
        let mut body = String::new();
        if !self.acceptance_criteria.is_empty() {
            body.push_str(&format!("## {}\n\n", SECTION_AC));
            for item in &self.acceptance_criteria.0 {
                body.push_str(&format!("- [ ] {}\n", item));
            }
        }
        body
    }

    fn doc_frontmatter(&self) -> Vec<(String, FmValue)> {
        let mut m = Vec::new();
        m.push(("id".into(), FmValue::Text(self.id.clone())));
        m.push(("parent-id".into(), FmValue::Text(self.parent_id.clone())));
        m.push(("title".into(), FmValue::Text(self.title.clone())));
        m.push(("status".into(), FmValue::Text(format!("{:?}", self.status()))));
        m.push(("dependencies".into(), FmValue::List(self.dependencies.clone())));
        if let Some(at) = self.activated_at {
            m.push(("activated-at".into(), FmValue::Text(millis_to_iso(at))));
        }
        m.push((
            "acceptance-criteria".into(),
            FmValue::List(self.acceptance_criteria.0.clone()),
        ));
        m.push(("created-at".into(), FmValue::Text(millis_to_iso(self.created_at))));
        m.push(("updated-at".into(), FmValue::Text(millis_to_iso(self.updated_at))));
        m.push(("children".into(), FmValue::List(vec![])));
        m
    }
}

impl Record for Phase {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "phases"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m.insert("parent_id".into(), IndexValue::String(self.parent_id.clone()));
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use taskstore::record::{IndexValue, Record};

    #[test]
    fn test_phase_new() {
        let phase = Phase::new("spec-123".to_string(), "Token generation".to_string());
        assert_eq!(phase.parent_id, "spec-123");
        assert_eq!(phase.title, "Token generation");
        assert_eq!(phase.status(), HierarchyStatus::Draft);
        assert!(!phase.id.is_empty());
        assert!(phase.created_at > 0);
        assert_eq!(phase.created_at, phase.updated_at);
    }

    #[test]
    fn test_phase_serde_roundtrip() {
        let phase = Phase::new("spec-456".to_string(), "Roundtrip Phase".to_string());
        let json = serde_json::to_string(&phase).unwrap();
        let deserialized: Phase = serde_json::from_str(&json).unwrap();
        assert_eq!(phase.id, deserialized.id);
        assert_eq!(phase.parent_id, deserialized.parent_id);
        assert_eq!(phase.title, deserialized.title);
        assert_eq!(phase.status(), deserialized.status());
        assert_eq!(phase.created_at, deserialized.created_at);
        assert_eq!(phase.updated_at, deserialized.updated_at);
    }

    #[test]
    fn test_phase_unique_ids() {
        let p1 = Phase::new("spec-1".to_string(), "A".to_string());
        let p2 = Phase::new("spec-1".to_string(), "B".to_string());
        assert_ne!(p1.id, p2.id);
    }

    #[test]
    fn test_phase_preserves_parent_id() {
        let parent_id = "spec-789".to_string();
        let phase = Phase::new(parent_id.clone(), "Title".to_string());
        assert_eq!(phase.parent_id, parent_id);
    }

    // FSM transition validation tests are in src/fsm/tests.rs (runtime interpreter).

    // Record trait tests

    #[test]
    fn test_phase_record_id() {
        let phase = Phase::new("spec-1".into(), "T".into());
        assert_eq!(Record::id(&phase), phase.id);
    }

    #[test]
    fn test_phase_record_updated_at() {
        let phase = Phase::new("spec-1".into(), "T".into());
        assert_eq!(Record::updated_at(&phase), phase.updated_at);
    }

    #[test]
    fn test_phase_record_collection_name() {
        assert_eq!(Phase::collection_name(), "phases");
    }

    #[test]
    fn test_phase_record_indexed_fields() {
        let phase = Phase::new("spec-42".into(), "T".into());
        let fields = phase.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("draft".to_string())));
        assert_eq!(
            fields.get("parent_id"),
            Some(&IndexValue::String("spec-42".to_string()))
        );
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn test_phase_record_serde_roundtrip() {
        let phase = Phase::new("spec-rt".into(), "Roundtrip".into());
        let json = serde_json::to_string(&phase).unwrap();
        let restored: Phase = serde_json::from_str(&json).unwrap();
        assert_eq!(Record::id(&restored), Record::id(&phase));
        assert_eq!(Record::updated_at(&restored), Record::updated_at(&phase));
        assert_eq!(Phase::collection_name(), "phases");
    }
}
