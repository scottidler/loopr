use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use taskstore::record::{IndexValue, Record};

use crate::domain::plan::HierarchyStatus;
use crate::id;

/// Type alias so Phase can name its own status type.
pub type PhaseStatus = HierarchyStatus;

/// Implementation phase within a Spec. Ordered by `order` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub id: String,
    pub spec_id: String,
    pub title: String,
    pub description: String,
    pub order: u32,
    pub status: PhaseStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Phase {
    pub fn new(spec_id: String, title: String, description: String, order: u32) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            spec_id,
            title,
            description,
            order,
            status: PhaseStatus::Draft,
            created_at: now,
            updated_at: now,
        }
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
        m.insert("spec_id".into(), IndexValue::String(self.spec_id.clone()));
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plan::hierarchy_transitions;
    use crate::domain::role::Role;
    use crate::domain::transition::validate_transition;
    use taskstore::record::{IndexValue, Record};

    #[test]
    fn test_phase_new() {
        let phase = Phase::new(
            "spec-123".to_string(),
            "Token generation".to_string(),
            "Implement JWT token generation".to_string(),
            1,
        );
        assert_eq!(phase.spec_id, "spec-123");
        assert_eq!(phase.title, "Token generation");
        assert_eq!(phase.description, "Implement JWT token generation");
        assert_eq!(phase.order, 1);
        assert_eq!(phase.status, HierarchyStatus::Draft);
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
        assert_eq!(phase.spec_id, deserialized.spec_id);
        assert_eq!(phase.title, deserialized.title);
        assert_eq!(phase.description, deserialized.description);
        assert_eq!(phase.order, deserialized.order);
        assert_eq!(phase.status, deserialized.status);
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
    fn test_phase_preserves_spec_id() {
        let spec_id = "spec-789".to_string();
        let phase = Phase::new(spec_id.clone(), "Title".to_string(), "Desc".to_string(), 0);
        assert_eq!(phase.spec_id, spec_id);
    }

    #[test]
    fn test_phase_order_preserved() {
        let phase = Phase::new("spec-1".to_string(), "T".to_string(), "D".to_string(), 42);
        assert_eq!(phase.order, 42);
    }

    // Phase uses the same HierarchyStatus FSM as Plan/Spec — verify transitions work

    #[test]
    fn test_phase_valid_transition_draft_to_active() {
        let rules = hierarchy_transitions();
        let result = validate_transition(
            HierarchyStatus::Draft,
            HierarchyStatus::Active,
            Role::Coordinator,
            &rules,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_phase_valid_transition_active_to_complete() {
        let rules = hierarchy_transitions();
        let result = validate_transition(
            HierarchyStatus::Active,
            HierarchyStatus::Complete,
            Role::Coordinator,
            &rules,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_phase_valid_transition_to_abandoned() {
        let rules = hierarchy_transitions();
        assert!(
            validate_transition(
                HierarchyStatus::Draft,
                HierarchyStatus::Abandoned,
                Role::Coordinator,
                &rules,
            )
            .is_ok()
        );
        assert!(
            validate_transition(
                HierarchyStatus::Active,
                HierarchyStatus::Abandoned,
                Role::Coordinator,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_phase_invalid_transition_wrong_role() {
        let rules = hierarchy_transitions();
        let result = validate_transition(
            HierarchyStatus::Draft,
            HierarchyStatus::Active,
            Role::Implementer,
            &rules,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_phase_invalid_transition_reverse() {
        let rules = hierarchy_transitions();
        let result = validate_transition(
            HierarchyStatus::Complete,
            HierarchyStatus::Active,
            Role::Coordinator,
            &rules,
        );
        assert!(result.is_err());
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
        assert_eq!(
            fields.get("status"),
            Some(&IndexValue::String("draft".to_string()))
        );
        assert_eq!(
            fields.get("spec_id"),
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
