use serde::{Deserialize, Serialize};
use std::fmt;

use crate::domain::role::Role;
use crate::domain::transition::TransitionRule;
use crate::id;

/// Shared status enum for Plan, Spec, and Phase records.
/// All three use the same four-state machine with Coordinator-only transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HierarchyStatus {
    Draft,
    Active,
    Complete,
    Abandoned,
}

impl fmt::Display for HierarchyStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HierarchyStatus::Draft => write!(f, "Draft"),
            HierarchyStatus::Active => write!(f, "Active"),
            HierarchyStatus::Complete => write!(f, "Complete"),
            HierarchyStatus::Abandoned => write!(f, "Abandoned"),
        }
    }
}

/// Type aliases so each record can name its own status type.
pub type PlanStatus = HierarchyStatus;

/// Transition rules for HierarchyStatus (used by Plan, Spec, Phase).
/// All transitions require the Coordinator role.
pub fn hierarchy_transitions() -> Vec<TransitionRule<HierarchyStatus>> {
    vec![
        TransitionRule {
            from: HierarchyStatus::Draft,
            to: HierarchyStatus::Active,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: HierarchyStatus::Active,
            to: HierarchyStatus::Complete,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: HierarchyStatus::Draft,
            to: HierarchyStatus::Abandoned,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: HierarchyStatus::Active,
            to: HierarchyStatus::Abandoned,
            role: Some(Role::Coordinator),
        },
    ]
}

/// Top-level objective. Contains markdown description and acceptance criteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: String,
    pub status: PlanStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Plan {
    pub fn new(title: String, description: String, acceptance_criteria: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            title,
            description,
            acceptance_criteria,
            status: HierarchyStatus::Draft,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transition::validate_transition;

    // --- HierarchyStatus tests ---

    #[test]
    fn test_hierarchy_status_display() {
        assert_eq!(HierarchyStatus::Draft.to_string(), "Draft");
        assert_eq!(HierarchyStatus::Active.to_string(), "Active");
        assert_eq!(HierarchyStatus::Complete.to_string(), "Complete");
        assert_eq!(HierarchyStatus::Abandoned.to_string(), "Abandoned");
    }

    #[test]
    fn test_hierarchy_status_serde_roundtrip() {
        for status in [
            HierarchyStatus::Draft,
            HierarchyStatus::Active,
            HierarchyStatus::Complete,
            HierarchyStatus::Abandoned,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: HierarchyStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_hierarchy_status_serde_format() {
        assert_eq!(serde_json::to_string(&HierarchyStatus::Draft).unwrap(), "\"draft\"");
        assert_eq!(serde_json::to_string(&HierarchyStatus::Active).unwrap(), "\"active\"");
        assert_eq!(
            serde_json::to_string(&HierarchyStatus::Complete).unwrap(),
            "\"complete\""
        );
        assert_eq!(
            serde_json::to_string(&HierarchyStatus::Abandoned).unwrap(),
            "\"abandoned\""
        );
    }

    // --- HierarchyStatus transition tests ---

    #[test]
    fn test_valid_transition_draft_to_active() {
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
    fn test_valid_transition_active_to_complete() {
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
    fn test_valid_transition_draft_to_abandoned() {
        let rules = hierarchy_transitions();
        let result = validate_transition(
            HierarchyStatus::Draft,
            HierarchyStatus::Abandoned,
            Role::Coordinator,
            &rules,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_valid_transition_active_to_abandoned() {
        let rules = hierarchy_transitions();
        let result = validate_transition(
            HierarchyStatus::Active,
            HierarchyStatus::Abandoned,
            Role::Coordinator,
            &rules,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_transition_complete_to_active() {
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
    fn test_invalid_transition_abandoned_to_active() {
        let rules = hierarchy_transitions();
        let result = validate_transition(
            HierarchyStatus::Abandoned,
            HierarchyStatus::Active,
            Role::Coordinator,
            &rules,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_transition_wrong_role() {
        let rules = hierarchy_transitions();
        // Implementer cannot make hierarchy transitions
        let result = validate_transition(
            HierarchyStatus::Draft,
            HierarchyStatus::Active,
            Role::Implementer,
            &rules,
        );
        assert!(result.is_err());
        // Integrator cannot make hierarchy transitions
        let result = validate_transition(
            HierarchyStatus::Draft,
            HierarchyStatus::Active,
            Role::Integrator,
            &rules,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_transition_draft_to_complete() {
        let rules = hierarchy_transitions();
        // Cannot skip Active — must go Draft → Active → Complete
        let result = validate_transition(
            HierarchyStatus::Draft,
            HierarchyStatus::Complete,
            Role::Coordinator,
            &rules,
        );
        assert!(result.is_err());
    }

    // --- Plan struct tests ---

    #[test]
    fn test_plan_new() {
        let plan = Plan::new(
            "Test Plan".to_string(),
            "A test plan".to_string(),
            "It works".to_string(),
        );
        assert_eq!(plan.title, "Test Plan");
        assert_eq!(plan.description, "A test plan");
        assert_eq!(plan.acceptance_criteria, "It works");
        assert_eq!(plan.status, HierarchyStatus::Draft);
        assert!(!plan.id.is_empty());
        assert!(plan.created_at > 0);
        assert_eq!(plan.created_at, plan.updated_at);
    }

    #[test]
    fn test_plan_serde_roundtrip() {
        let plan = Plan::new(
            "Test Plan".to_string(),
            "Description".to_string(),
            "Criteria".to_string(),
        );
        let json = serde_json::to_string(&plan).unwrap();
        let deserialized: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.id, deserialized.id);
        assert_eq!(plan.title, deserialized.title);
        assert_eq!(plan.status, deserialized.status);
        assert_eq!(plan.created_at, deserialized.created_at);
    }

    #[test]
    fn test_plan_unique_ids() {
        let p1 = Plan::new("A".to_string(), "".to_string(), "".to_string());
        let p2 = Plan::new("B".to_string(), "".to_string(), "".to_string());
        assert_ne!(p1.id, p2.id);
    }
}
