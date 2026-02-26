use serde::{Deserialize, Serialize};
use std::fmt;

use crate::id;

/// Scope at which a learning applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LearningScope {
    WorkItem,
    Phase,
    Spec,
    Plan,
    Global,
}

impl fmt::Display for LearningScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LearningScope::WorkItem => write!(f, "WorkItem"),
            LearningScope::Phase => write!(f, "Phase"),
            LearningScope::Spec => write!(f, "Spec"),
            LearningScope::Plan => write!(f, "Plan"),
            LearningScope::Global => write!(f, "Global"),
        }
    }
}

/// Insight captured during work. Can be promoted to Policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Learning {
    pub id: String,
    pub source_id: String,
    pub scope: LearningScope,
    pub content: String,
    pub reinforcements: u32,
    pub contradictions: u32,
    pub promoted: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Learning {
    pub fn new(source_id: String, scope: LearningScope, content: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            source_id,
            scope,
            content,
            reinforcements: 0,
            contradictions: 0,
            promoted: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// Record an independent confirmation of this learning.
    pub fn reinforce(&mut self) {
        self.reinforcements += 1;
        self.updated_at = id::now_millis();
    }

    /// Record a contradiction of this learning.
    pub fn contradict(&mut self) {
        self.contradictions += 1;
        self.updated_at = id::now_millis();
    }

    /// Promote this learning to a policy.
    pub fn promote(&mut self) {
        self.promoted = true;
        self.updated_at = id::now_millis();
    }

    /// Demote this learning from policy status.
    pub fn demote(&mut self) {
        self.promoted = false;
        self.updated_at = id::now_millis();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- LearningScope tests ---

    #[test]
    fn test_learning_scope_display() {
        assert_eq!(LearningScope::WorkItem.to_string(), "WorkItem");
        assert_eq!(LearningScope::Phase.to_string(), "Phase");
        assert_eq!(LearningScope::Spec.to_string(), "Spec");
        assert_eq!(LearningScope::Plan.to_string(), "Plan");
        assert_eq!(LearningScope::Global.to_string(), "Global");
    }

    #[test]
    fn test_learning_scope_serde_roundtrip() {
        for scope in [
            LearningScope::WorkItem,
            LearningScope::Phase,
            LearningScope::Spec,
            LearningScope::Plan,
            LearningScope::Global,
        ] {
            let json = serde_json::to_string(&scope).unwrap();
            let deserialized: LearningScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, deserialized);
        }
    }

    #[test]
    fn test_learning_scope_serde_format() {
        assert_eq!(
            serde_json::to_string(&LearningScope::WorkItem).unwrap(),
            "\"workitem\""
        );
        assert_eq!(
            serde_json::to_string(&LearningScope::Phase).unwrap(),
            "\"phase\""
        );
        assert_eq!(
            serde_json::to_string(&LearningScope::Spec).unwrap(),
            "\"spec\""
        );
        assert_eq!(
            serde_json::to_string(&LearningScope::Plan).unwrap(),
            "\"plan\""
        );
        assert_eq!(
            serde_json::to_string(&LearningScope::Global).unwrap(),
            "\"global\""
        );
    }

    // --- Learning struct tests ---

    #[test]
    fn test_learning_new() {
        let learning = Learning::new(
            "wi-123".to_string(),
            LearningScope::WorkItem,
            "Always run tests before committing".to_string(),
        );
        assert_eq!(learning.source_id, "wi-123");
        assert_eq!(learning.scope, LearningScope::WorkItem);
        assert_eq!(learning.content, "Always run tests before committing");
        assert_eq!(learning.reinforcements, 0);
        assert_eq!(learning.contradictions, 0);
        assert!(!learning.promoted);
        assert!(!learning.id.is_empty());
        assert!(learning.created_at > 0);
        assert_eq!(learning.created_at, learning.updated_at);
    }

    #[test]
    fn test_learning_serde_roundtrip() {
        let learning = Learning::new(
            "phase-456".to_string(),
            LearningScope::Phase,
            "Split large tasks into smaller ones".to_string(),
        );
        let json = serde_json::to_string(&learning).unwrap();
        let deserialized: Learning = serde_json::from_str(&json).unwrap();
        assert_eq!(learning.id, deserialized.id);
        assert_eq!(learning.source_id, deserialized.source_id);
        assert_eq!(learning.scope, deserialized.scope);
        assert_eq!(learning.content, deserialized.content);
        assert_eq!(learning.reinforcements, deserialized.reinforcements);
        assert_eq!(learning.contradictions, deserialized.contradictions);
        assert_eq!(learning.promoted, deserialized.promoted);
        assert_eq!(learning.created_at, deserialized.created_at);
    }

    #[test]
    fn test_learning_unique_ids() {
        let l1 = Learning::new("a".to_string(), LearningScope::Global, "x".to_string());
        let l2 = Learning::new("a".to_string(), LearningScope::Global, "y".to_string());
        assert_ne!(l1.id, l2.id);
    }

    #[test]
    fn test_learning_reinforce() {
        let mut learning = Learning::new(
            "wi-1".to_string(),
            LearningScope::WorkItem,
            "insight".to_string(),
        );
        assert_eq!(learning.reinforcements, 0);
        learning.reinforce();
        assert_eq!(learning.reinforcements, 1);
        learning.reinforce();
        assert_eq!(learning.reinforcements, 2);
    }

    #[test]
    fn test_learning_contradict() {
        let mut learning = Learning::new(
            "wi-1".to_string(),
            LearningScope::WorkItem,
            "insight".to_string(),
        );
        assert_eq!(learning.contradictions, 0);
        learning.contradict();
        assert_eq!(learning.contradictions, 1);
        learning.contradict();
        assert_eq!(learning.contradictions, 2);
    }

    #[test]
    fn test_learning_promote_demote() {
        let mut learning = Learning::new(
            "plan-1".to_string(),
            LearningScope::Plan,
            "policy candidate".to_string(),
        );
        assert!(!learning.promoted);
        learning.promote();
        assert!(learning.promoted);
        learning.demote();
        assert!(!learning.promoted);
    }

    #[test]
    fn test_learning_source_id_preserved() {
        let learning = Learning::new(
            "spec-789".to_string(),
            LearningScope::Spec,
            "content".to_string(),
        );
        assert_eq!(learning.source_id, "spec-789");
    }

    #[test]
    fn test_learning_global_scope() {
        let learning = Learning::new(
            "global".to_string(),
            LearningScope::Global,
            "Global insight".to_string(),
        );
        assert_eq!(learning.scope, LearningScope::Global);
        assert_eq!(learning.scope.to_string(), "Global");
    }
}
