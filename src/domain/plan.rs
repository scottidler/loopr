use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use taskstore::record::{IndexValue, Record};

use crate::domain::role::Role;
use crate::domain::transition::TransitionRule;
use crate::id;

/// Shared status enum for Plan, Spec, and Phase records.
/// All three use the same four-state machine with Coordinator-only transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HierarchyStatus {
    #[serde(alias = "Draft")]
    Draft,
    #[serde(alias = "Active")]
    Active,
    #[serde(alias = "Complete")]
    Complete,
    #[serde(alias = "Abandoned")]
    Abandoned,
}

impl fmt::Display for HierarchyStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HierarchyStatus::Draft => write!(f, "draft"),
            HierarchyStatus::Active => write!(f, "active"),
            HierarchyStatus::Complete => write!(f, "complete"),
            HierarchyStatus::Abandoned => write!(f, "abandoned"),
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
        log::debug!("Plan::new(title={})", title);
        let now = id::now_millis();
        Self {
            id: id::generate_id("pl"),
            title,
            description,
            acceptance_criteria,
            status: HierarchyStatus::Draft,
            created_at: now,
            updated_at: now,
        }
    }
}

impl Record for Plan {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "plans"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transition::validate_transition;

    // --- HierarchyStatus tests ---

    #[test]
    fn test_hierarchy_status_display() {
        assert_eq!(HierarchyStatus::Draft.to_string(), "draft");
        assert_eq!(HierarchyStatus::Active.to_string(), "active");
        assert_eq!(HierarchyStatus::Complete.to_string(), "complete");
        assert_eq!(HierarchyStatus::Abandoned.to_string(), "abandoned");
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

    #[test]
    fn test_hierarchy_status_pascal_case_aliases() {
        for (json, expected) in [
            ("\"Draft\"", HierarchyStatus::Draft),
            ("\"Active\"", HierarchyStatus::Active),
            ("\"Complete\"", HierarchyStatus::Complete),
            ("\"Abandoned\"", HierarchyStatus::Abandoned),
        ] {
            let deserialized: HierarchyStatus = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("PascalCase '{}' should deserialize: {}", json, e));
            assert_eq!(deserialized, expected);
        }
    }

    #[test]
    fn test_hierarchy_status_display_matches_serde() {
        // Regression: Display must produce values that serde can deserialize.
        // CLI dispatch uses to_string() but handlers use serde_json::from_value().
        for status in [
            HierarchyStatus::Draft,
            HierarchyStatus::Active,
            HierarchyStatus::Complete,
            HierarchyStatus::Abandoned,
        ] {
            let display = status.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: HierarchyStatus = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("Display output '{}' not deserializable: {}", display, e));
            assert_eq!(status, deserialized);
        }
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

    // --- Record trait tests ---

    #[test]
    fn test_plan_record_id() {
        let plan = Plan::new("Test".to_string(), "Desc".to_string(), "Crit".to_string());
        assert_eq!(Record::id(&plan), plan.id.as_str());
    }

    #[test]
    fn test_plan_record_updated_at() {
        let plan = Plan::new("Test".to_string(), "Desc".to_string(), "Crit".to_string());
        assert_eq!(Record::updated_at(&plan), plan.updated_at);
    }

    #[test]
    fn test_plan_record_collection_name() {
        assert_eq!(Plan::collection_name(), "plans");
    }

    #[test]
    fn test_plan_record_indexed_fields() {
        let plan = Plan::new("Test".to_string(), "Desc".to_string(), "Crit".to_string());
        let fields = plan.indexed_fields();
        assert_eq!(fields.len(), 1);
        assert_eq!(
            fields.get("status"),
            Some(&taskstore::record::IndexValue::String("draft".to_string()))
        );
    }

    #[test]
    fn test_plan_record_roundtrip_json() {
        let plan = Plan::new("RT".to_string(), "Desc".to_string(), "Crit".to_string());
        let json = serde_json::to_string(&plan).unwrap();
        let restored: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(Record::id(&restored), Record::id(&plan));
        assert_eq!(Record::updated_at(&restored), Record::updated_at(&plan));
        assert_eq!(Plan::collection_name(), "plans");
    }
}
