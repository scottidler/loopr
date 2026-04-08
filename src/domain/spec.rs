use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use taskstore::record::{IndexValue, Record};

use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::markdown::{DocMarkdown, FmValue, millis_to_iso};
use crate::domain::plan::HierarchyStatus;
use crate::id;

/// Type alias so Spec can name its own status type.
pub type SpecStatus = HierarchyStatus;

/// Detailed specification derived from a Plan. Ordered by `order` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    status: SpecStatus,
    #[serde(default)]
    pub order: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Spec {
    /// Read current status.
    pub fn status(&self) -> SpecStatus {
        self.status
    }

    /// Validated FSM transition. Returns Err if invalid.
    pub fn transition(
        &mut self,
        target: SpecStatus,
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
    pub fn force_status(&mut self, target: SpecStatus) {
        self.status = target;
        self.updated_at = crate::id::now_millis();
    }

    pub fn new(parent_id: String, title: String, order: u32) -> Self {
        tracing::debug!("Spec::new(parent_id={}, title={}, order={})", parent_id, title, order);
        let now = id::now_millis();
        Self {
            id: id::generate_id("sp"),
            parent_id,
            title,
            acceptance_criteria: AcceptanceCriteria::default(),
            status: HierarchyStatus::Draft,
            order,
            created_at: now,
            updated_at: now,
        }
    }
}

impl DocMarkdown for Spec {
    fn doc_id(&self) -> &str {
        &self.id
    }

    fn doc_body(&self) -> String {
        let mut body = String::new();
        if !self.acceptance_criteria.is_empty() {
            body.push_str("## Acceptance Criteria\n\n");
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

impl Record for Spec {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "specs"
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
    fn test_spec_new() {
        let spec = Spec::new("plan-123".to_string(), "Test Spec".to_string(), 0);
        assert_eq!(spec.parent_id, "plan-123");
        assert_eq!(spec.title, "Test Spec");
        assert_eq!(spec.status(), HierarchyStatus::Draft);
        assert_eq!(spec.order, 0);
        assert!(!spec.id.is_empty());
        assert!(spec.created_at > 0);
        assert_eq!(spec.created_at, spec.updated_at);
    }

    #[test]
    fn test_spec_serde_roundtrip() {
        let spec = Spec::new("plan-456".to_string(), "Roundtrip Spec".to_string(), 1);
        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: Spec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec.id, deserialized.id);
        assert_eq!(spec.parent_id, deserialized.parent_id);
        assert_eq!(spec.title, deserialized.title);
        assert_eq!(spec.status(), deserialized.status());
        assert_eq!(spec.order, deserialized.order);
        assert_eq!(spec.created_at, deserialized.created_at);
        assert_eq!(spec.updated_at, deserialized.updated_at);
    }

    #[test]
    fn test_spec_order_preserved() {
        let spec = Spec::new("plan-1".to_string(), "T".to_string(), 42);
        assert_eq!(spec.order, 42);
    }

    #[test]
    fn test_spec_unique_ids() {
        let s1 = Spec::new("plan-1".to_string(), "A".to_string(), 0);
        let s2 = Spec::new("plan-1".to_string(), "B".to_string(), 1);
        assert_ne!(s1.id, s2.id);
    }

    #[test]
    fn test_spec_preserves_parent_id() {
        let parent_id = "plan-789".to_string();
        let spec = Spec::new(parent_id.clone(), "Title".to_string(), 0);
        assert_eq!(spec.parent_id, parent_id);
    }

    // Spec uses the same HierarchyStatus FSM as Plan - verify transitions work for Spec context

    #[test]
    fn test_spec_valid_transition_draft_to_active() {
        let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Active, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Changed);
    }

    #[test]
    fn test_spec_valid_transition_active_to_complete() {
        let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Complete, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Changed);
    }

    #[test]
    fn test_spec_valid_transition_to_abandoned() {
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
    fn test_spec_invalid_transition_wrong_role() {
        assert!(
            HierarchyStatus::Draft
                .validate_transition(HierarchyStatus::Active, Role::Implementer)
                .is_err()
        );
    }

    #[test]
    fn test_spec_invalid_transition_reverse() {
        assert!(
            HierarchyStatus::Complete
                .validate_transition(HierarchyStatus::Active, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_spec_invalid_transition_skip_state() {
        assert!(
            HierarchyStatus::Draft
                .validate_transition(HierarchyStatus::Complete, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_spec_idempotent_self_transition() {
        let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Draft, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Unchanged);
    }

    // Record trait tests

    #[test]
    fn test_spec_record_id() {
        let spec = Spec::new("plan-1".to_string(), "T".to_string(), 0);
        assert_eq!(Record::id(&spec), spec.id);
    }

    #[test]
    fn test_spec_record_updated_at() {
        let spec = Spec::new("plan-1".to_string(), "T".to_string(), 0);
        assert_eq!(Record::updated_at(&spec), spec.updated_at);
    }

    #[test]
    fn test_spec_record_collection_name() {
        assert_eq!(Spec::collection_name(), "specs");
    }

    #[test]
    fn test_spec_record_indexed_fields() {
        let spec = Spec::new("plan-42".to_string(), "T".to_string(), 0);
        let fields = spec.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("draft".to_string())));
        assert_eq!(
            fields.get("parent_id"),
            Some(&IndexValue::String("plan-42".to_string()))
        );
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn test_spec_record_roundtrip_via_serde() {
        let spec = Spec::new("plan-99".to_string(), "RT".to_string(), 2);
        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: Spec = serde_json::from_str(&json).unwrap();
        assert_eq!(Record::id(&spec), Record::id(&deserialized));
        assert_eq!(Record::updated_at(&spec), Record::updated_at(&deserialized));
        assert_eq!(
            spec.indexed_fields().get("status"),
            deserialized.indexed_fields().get("status")
        );
        assert_eq!(
            spec.indexed_fields().get("parent_id"),
            deserialized.indexed_fields().get("parent_id")
        );
    }
}
