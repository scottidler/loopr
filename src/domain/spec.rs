use serde::{Deserialize, Serialize};

use crate::domain::plan::HierarchyStatus;
use crate::id;

/// Type alias so Spec can name its own status type.
pub type SpecStatus = HierarchyStatus;

/// Detailed specification derived from a Plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub id: String,
    pub plan_id: String,
    pub title: String,
    pub description: String,
    pub status: SpecStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Spec {
    pub fn new(plan_id: String, title: String, description: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            plan_id,
            title,
            description,
            status: HierarchyStatus::Draft,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plan::hierarchy_transitions;
    use crate::domain::role::Role;
    use crate::domain::transition::validate_transition;

    #[test]
    fn test_spec_new() {
        let spec = Spec::new(
            "plan-123".to_string(),
            "Test Spec".to_string(),
            "A detailed specification".to_string(),
        );
        assert_eq!(spec.plan_id, "plan-123");
        assert_eq!(spec.title, "Test Spec");
        assert_eq!(spec.description, "A detailed specification");
        assert_eq!(spec.status, HierarchyStatus::Draft);
        assert!(!spec.id.is_empty());
        assert!(spec.created_at > 0);
        assert_eq!(spec.created_at, spec.updated_at);
    }

    #[test]
    fn test_spec_serde_roundtrip() {
        let spec = Spec::new(
            "plan-456".to_string(),
            "Roundtrip Spec".to_string(),
            "Description".to_string(),
        );
        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: Spec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec.id, deserialized.id);
        assert_eq!(spec.plan_id, deserialized.plan_id);
        assert_eq!(spec.title, deserialized.title);
        assert_eq!(spec.description, deserialized.description);
        assert_eq!(spec.status, deserialized.status);
        assert_eq!(spec.created_at, deserialized.created_at);
        assert_eq!(spec.updated_at, deserialized.updated_at);
    }

    #[test]
    fn test_spec_unique_ids() {
        let s1 = Spec::new("plan-1".to_string(), "A".to_string(), "".to_string());
        let s2 = Spec::new("plan-1".to_string(), "B".to_string(), "".to_string());
        assert_ne!(s1.id, s2.id);
    }

    #[test]
    fn test_spec_preserves_plan_id() {
        let plan_id = "plan-789".to_string();
        let spec = Spec::new(plan_id.clone(), "Title".to_string(), "Desc".to_string());
        assert_eq!(spec.plan_id, plan_id);
    }

    // Spec uses the same HierarchyStatus FSM as Plan — verify transitions work for Spec context

    #[test]
    fn test_spec_valid_transition_draft_to_active() {
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
    fn test_spec_valid_transition_active_to_complete() {
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
    fn test_spec_valid_transition_to_abandoned() {
        let rules = hierarchy_transitions();
        // From Draft
        assert!(
            validate_transition(
                HierarchyStatus::Draft,
                HierarchyStatus::Abandoned,
                Role::Coordinator,
                &rules,
            )
            .is_ok()
        );
        // From Active
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
    fn test_spec_invalid_transition_wrong_role() {
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
    fn test_spec_invalid_transition_reverse() {
        let rules = hierarchy_transitions();
        let result = validate_transition(
            HierarchyStatus::Complete,
            HierarchyStatus::Active,
            Role::Coordinator,
            &rules,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_spec_invalid_transition_skip_state() {
        let rules = hierarchy_transitions();
        let result = validate_transition(
            HierarchyStatus::Draft,
            HierarchyStatus::Complete,
            Role::Coordinator,
            &rules,
        );
        assert!(result.is_err());
    }
}
