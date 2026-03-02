use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use crate::id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalStatus {
    Draft,
    Open,
    Accepted,
    Rejected,
    Withdrawn,
}

impl std::fmt::Display for ProposalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub title: String,
    pub description: String,
    pub author_id: String,
    pub status: ProposalStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Proposal {
    pub fn new(title: String, description: String, author_id: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id("pr"),
            title,
            description,
            author_id,
            status: ProposalStatus::Draft,
            created_at: now,
            updated_at: now,
        }
    }
}

impl Record for Proposal {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "proposals"
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
    fn test_proposal_new() {
        let p = Proposal::new("Test".into(), "Desc".into(), "author-1".into());
        assert_eq!(p.status, ProposalStatus::Draft);
        assert!(!p.id.is_empty());
    }

    #[test]
    fn test_proposal_serde_roundtrip() {
        let p = Proposal::new("Test".into(), "Desc".into(), "author-1".into());
        let json = serde_json::to_string(&p).unwrap();
        let d: Proposal = serde_json::from_str(&json).unwrap();
        assert_eq!(p.id, d.id);
        assert_eq!(p.title, d.title);
    }

    #[test]
    fn test_proposal_status_display() {
        assert_eq!(ProposalStatus::Draft.to_string(), "Draft");
        assert_eq!(ProposalStatus::Open.to_string(), "Open");
    }

    #[test]
    fn test_proposal_record_collection_name() {
        assert_eq!(Proposal::collection_name(), "proposals");
    }

    #[test]
    fn test_proposal_record_id() {
        let p = Proposal::new("Title".into(), "Desc".into(), "author-1".into());
        assert_eq!(Record::id(&p), &p.id);
    }

    #[test]
    fn test_proposal_record_updated_at() {
        let p = Proposal::new("Title".into(), "Desc".into(), "author-1".into());
        assert_eq!(Record::updated_at(&p), p.updated_at);
    }

    #[test]
    fn test_proposal_record_indexed_fields() {
        let p = Proposal::new("Title".into(), "Desc".into(), "author-1".into());
        let fields = Record::indexed_fields(&p);
        assert!(fields.contains_key("status"));
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Draft".to_string())));
    }
}
