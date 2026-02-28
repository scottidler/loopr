use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use crate::id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionStatus {
    Pending,
    Decided,
    Superseded,
}

impl std::fmt::Display for DecisionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub proposal_id: Option<String>,
    pub title: String,
    pub rationale: String,
    pub decided_by: String,
    pub status: DecisionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Decision {
    pub fn new(title: String, rationale: String, decided_by: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            proposal_id: None,
            title,
            rationale,
            decided_by,
            status: DecisionStatus::Pending,
            created_at: now,
            updated_at: now,
        }
    }
}

impl Record for Decision {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "decisions"
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

    #[test]
    fn test_decision_new() {
        let d = Decision::new("Use JWT".into(), "Industry standard".into(), "coord-1".into());
        assert_eq!(d.status, DecisionStatus::Pending);
        assert!(d.proposal_id.is_none());
        assert!(!d.id.is_empty());
    }

    #[test]
    fn test_decision_serde_roundtrip() {
        let d = Decision::new("Use JWT".into(), "Reason".into(), "coord-1".into());
        let json = serde_json::to_string(&d).unwrap();
        let de: Decision = serde_json::from_str(&json).unwrap();
        assert_eq!(d.id, de.id);
        assert_eq!(d.title, de.title);
    }

    #[test]
    fn test_decision_status_display() {
        assert_eq!(DecisionStatus::Pending.to_string(), "Pending");
        assert_eq!(DecisionStatus::Decided.to_string(), "Decided");
    }

    #[test]
    fn test_decision_record_collection_name() {
        assert_eq!(Decision::collection_name(), "decisions");
    }

    #[test]
    fn test_decision_record_id() {
        let d = Decision::new("Title".into(), "Rationale".into(), "decider-1".into());
        assert_eq!(Record::id(&d), &d.id);
    }

    #[test]
    fn test_decision_record_updated_at() {
        let d = Decision::new("Title".into(), "Rationale".into(), "decider-1".into());
        assert_eq!(Record::updated_at(&d), d.updated_at);
    }

    #[test]
    fn test_decision_record_indexed_fields() {
        let d = Decision::new("Title".into(), "Rationale".into(), "decider-1".into());
        let fields = Record::indexed_fields(&d);
        assert!(fields.contains_key("status"));
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Pending".to_string())));
    }
}
