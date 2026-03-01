use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use crate::id;

/// A singleton record representing the Coordinator's current goal.
/// Persisted in TaskStore so it survives daemon crashes.
/// Only one active goal exists at a time; setting a new goal deactivates the previous one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorGoal {
    pub id: String,
    pub goal: String,
    pub active: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl CoordinatorGoal {
    pub fn new(goal: String) -> Self {
        log::debug!("CoordinatorGoal::new(goal={})", goal);
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            goal,
            active: true,
            created_at: now,
            updated_at: now,
        }
    }

    /// Deactivate this goal.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.updated_at = id::now_millis();
    }
}

impl Record for CoordinatorGoal {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "coordinator_goals"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("active".into(), IndexValue::String(self.active.to_string()));
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinator_goal_new() {
        let goal = CoordinatorGoal::new("Build auth system".to_string());
        assert!(!goal.id.is_empty());
        assert_eq!(goal.goal, "Build auth system");
        assert!(goal.active);
        assert!(goal.created_at > 0);
        assert_eq!(goal.created_at, goal.updated_at);
    }

    #[test]
    fn test_coordinator_goal_unique_ids() {
        let g1 = CoordinatorGoal::new("goal 1".to_string());
        let g2 = CoordinatorGoal::new("goal 2".to_string());
        assert_ne!(g1.id, g2.id);
    }

    #[test]
    fn test_coordinator_goal_deactivate() {
        let mut goal = CoordinatorGoal::new("Build something".to_string());
        assert!(goal.active);
        goal.deactivate();
        assert!(!goal.active);
        assert!(goal.updated_at >= goal.created_at);
    }

    #[test]
    fn test_coordinator_goal_serde_roundtrip() {
        let goal = CoordinatorGoal::new("Test goal".to_string());
        let json = serde_json::to_string(&goal).unwrap();
        let deserialized: CoordinatorGoal = serde_json::from_str(&json).unwrap();
        assert_eq!(goal.id, deserialized.id);
        assert_eq!(goal.goal, deserialized.goal);
        assert_eq!(goal.active, deserialized.active);
        assert_eq!(goal.created_at, deserialized.created_at);
        assert_eq!(goal.updated_at, deserialized.updated_at);
    }

    #[test]
    fn test_record_id() {
        let goal = CoordinatorGoal::new("g".to_string());
        assert_eq!(Record::id(&goal), goal.id);
    }

    #[test]
    fn test_record_updated_at() {
        let goal = CoordinatorGoal::new("g".to_string());
        assert_eq!(Record::updated_at(&goal), goal.updated_at);
    }

    #[test]
    fn test_record_collection_name() {
        assert_eq!(CoordinatorGoal::collection_name(), "coordinator_goals");
    }

    #[test]
    fn test_record_indexed_fields() {
        let goal = CoordinatorGoal::new("g".to_string());
        let fields = goal.indexed_fields();
        assert_eq!(fields.get("active"), Some(&IndexValue::String("true".to_string())));
        assert_eq!(fields.len(), 1);
    }

    #[test]
    fn test_record_indexed_fields_after_deactivate() {
        let mut goal = CoordinatorGoal::new("g".to_string());
        goal.deactivate();
        let fields = goal.indexed_fields();
        assert_eq!(fields.get("active"), Some(&IndexValue::String("false".to_string())));
    }
}
