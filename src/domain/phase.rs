use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use taskstore::record::{IndexValue, Record};

use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::markdown::{DocMarkdown, FmValue, millis_to_iso};
use crate::domain::plan::HierarchyStatus;
use crate::id;

/// Type alias so Phase can name its own status type.
pub type PhaseStatus = HierarchyStatus;

/// Implementation phase within a Spec. Ordered by `order` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub description: String,
    pub order: u32,
    status: PhaseStatus,
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    /// Kept for backward deserialization of old JSONL records; ignored on write.
    #[serde(default, skip_serializing)]
    pub validation_commands: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Phase {
    /// Read current status.
    pub fn status(&self) -> PhaseStatus {
        self.status
    }

    /// Validated FSM transition. Returns Err if invalid.
    pub fn transition(
        &mut self,
        target: PhaseStatus,
        role: crate::domain::role::Role,
    ) -> crate::error::Result<crate::domain::transition::Transition> {
        let result = self.status.validate_transition(target, role)?;
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

    pub fn new(parent_id: String, title: String, description: String, order: u32) -> Self {
        tracing::debug!("Phase::new(parent_id={}, title={}, order={})", parent_id, title, order);
        let now = id::now_millis();
        Self {
            id: id::generate_id("ph"),
            parent_id,
            title,
            description,
            order,
            status: PhaseStatus::Draft,
            acceptance_criteria: AcceptanceCriteria::default(),
            validation_commands: Vec::new(),
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
        let mut body = self.description.clone();
        if !self.acceptance_criteria.is_empty() {
            body.push_str("\n\n## Acceptance Criteria\n\n");
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
        m.push(("order".into(), FmValue::Text(self.order.to_string())));
        m.push((
            "acceptance-criteria".into(),
            FmValue::List(self.acceptance_criteria.0.clone()),
        ));
        m.push(("created-at".into(), FmValue::Text(millis_to_iso(self.created_at))));
        m.push(("updated-at".into(), FmValue::Text(millis_to_iso(self.updated_at))));
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
    use crate::domain::role::Role;
    use crate::domain::transition::Transition;
    use taskstore::record::{IndexValue, Record};

    #[test]
    fn test_phase_new() {
        let phase = Phase::new(
            "spec-123".to_string(),
            "Token generation".to_string(),
            "Implement JWT token generation".to_string(),
            1,
        );
        assert_eq!(phase.parent_id, "spec-123");
        assert_eq!(phase.title, "Token generation");
        assert_eq!(phase.description, "Implement JWT token generation");
        assert_eq!(phase.order, 1);
        assert_eq!(phase.status(), HierarchyStatus::Draft);
        assert!(!phase.id.is_empty());
        assert!(phase.created_at > 0);
        assert_eq!(phase.created_at, phase.updated_at);
    }

    #[test]
    fn test_phase_serde_roundtrip() {
        let phase = Phase::new(
            "spec-456".to_string(),
            "Roundtrip Phase".to_string(),
            "Description".to_string(),
            2,
        );
        let json = serde_json::to_string(&phase).unwrap();
        let deserialized: Phase = serde_json::from_str(&json).unwrap();
        assert_eq!(phase.id, deserialized.id);
        assert_eq!(phase.parent_id, deserialized.parent_id);
        assert_eq!(phase.title, deserialized.title);
        assert_eq!(phase.description, deserialized.description);
        assert_eq!(phase.order, deserialized.order);
        assert_eq!(phase.status(), deserialized.status());
        assert_eq!(phase.created_at, deserialized.created_at);
        assert_eq!(phase.updated_at, deserialized.updated_at);
    }

    #[test]
    fn test_phase_unique_ids() {
        let p1 = Phase::new("spec-1".to_string(), "A".to_string(), "".to_string(), 1);
        let p2 = Phase::new("spec-1".to_string(), "B".to_string(), "".to_string(), 2);
        assert_ne!(p1.id, p2.id);
    }

    #[test]
    fn test_phase_preserves_parent_id() {
        let parent_id = "spec-789".to_string();
        let phase = Phase::new(parent_id.clone(), "Title".to_string(), "Desc".to_string(), 0);
        assert_eq!(phase.parent_id, parent_id);
    }

    #[test]
    fn test_phase_order_preserved() {
        let phase = Phase::new("spec-1".to_string(), "T".to_string(), "D".to_string(), 42);
        assert_eq!(phase.order, 42);
    }

    // Phase uses the same HierarchyStatus FSM as Plan/Spec - verify transitions work

    #[test]
    fn test_phase_valid_transition_draft_to_active() {
        let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Active, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Changed);
    }

    #[test]
    fn test_phase_valid_transition_active_to_complete() {
        let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Complete, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Changed);
    }

    #[test]
    fn test_phase_valid_transition_to_abandoned() {
        assert!(
            HierarchyStatus::Draft
                .validate_transition(HierarchyStatus::Abandoned, Role::Coordinator)
                .is_ok()
        );
        assert!(
            HierarchyStatus::Active
                .validate_transition(HierarchyStatus::Abandoned, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_phase_invalid_transition_wrong_role() {
        assert!(
            HierarchyStatus::Draft
                .validate_transition(HierarchyStatus::Active, Role::Implementer)
                .is_err()
        );
    }

    #[test]
    fn test_phase_invalid_transition_reverse() {
        assert!(
            HierarchyStatus::Complete
                .validate_transition(HierarchyStatus::Active, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_phase_idempotent_self_transition() {
        let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Draft, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Unchanged);
    }

    // Record trait tests

    #[test]
    fn test_phase_record_id() {
        let phase = Phase::new("spec-1".into(), "T".into(), "D".into(), 1);
        assert_eq!(Record::id(&phase), phase.id);
    }

    #[test]
    fn test_phase_record_updated_at() {
        let phase = Phase::new("spec-1".into(), "T".into(), "D".into(), 1);
        assert_eq!(Record::updated_at(&phase), phase.updated_at);
    }

    #[test]
    fn test_phase_record_collection_name() {
        assert_eq!(Phase::collection_name(), "phases");
    }

    #[test]
    fn test_phase_record_indexed_fields() {
        let phase = Phase::new("spec-42".into(), "T".into(), "D".into(), 1);
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
        let phase = Phase::new("spec-rt".into(), "Roundtrip".into(), "D".into(), 3);
        let json = serde_json::to_string(&phase).unwrap();
        let restored: Phase = serde_json::from_str(&json).unwrap();
        assert_eq!(Record::id(&restored), Record::id(&phase));
        assert_eq!(Record::updated_at(&restored), Record::updated_at(&phase));
        assert_eq!(Phase::collection_name(), "phases");
    }
}
