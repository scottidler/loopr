use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use crate::domain::role::Role;
use crate::domain::transition::TransitionRule;
use crate::id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkItemStatus {
    Draft,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Integrated,
    Done,
    Abandoned,
}

impl std::fmt::Display for WorkItemStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Returns the FSM transition rules for WorkItem status.
pub fn work_item_transitions() -> Vec<TransitionRule<WorkItemStatus>> {
    use WorkItemStatus::*;
    vec![
        TransitionRule {
            from: Draft,
            to: Ready,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Ready,
            to: InProgress,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: InProgress,
            to: Blocked,
            role: None,
        },
        TransitionRule {
            from: Blocked,
            to: Ready,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: InProgress,
            to: InReview,
            role: Some(Role::Implementer),
        },
        TransitionRule {
            from: InReview,
            to: InProgress,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: InReview,
            to: Integrated,
            role: Some(Role::Integrator),
        },
        TransitionRule {
            from: Integrated,
            to: Done,
            role: Some(Role::Coordinator),
        },
        // Abandoned from any non-terminal state
        TransitionRule {
            from: Draft,
            to: Abandoned,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Ready,
            to: Abandoned,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: InProgress,
            to: Abandoned,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Blocked,
            to: Abandoned,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: InReview,
            to: Abandoned,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Integrated,
            to: Abandoned,
            role: Some(Role::Coordinator),
        },
    ]
}

/// Concrete unit of work within a Phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub phase_id: String,
    pub title: String,
    pub description: String,
    pub assignee: Option<String>,
    pub status: WorkItemStatus,
    pub resource_tags: Vec<String>,
    pub dependencies: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl WorkItem {
    pub fn new(phase_id: String, title: String, description: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            phase_id,
            title,
            description,
            assignee: None,
            status: WorkItemStatus::Draft,
            resource_tags: Vec::new(),
            dependencies: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

impl Record for WorkItem {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "work_items"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m.insert("phase_id".into(), IndexValue::String(self.phase_id.clone()));
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transition::validate_transition;

    #[test]
    fn test_work_item_status_display() {
        assert_eq!(WorkItemStatus::Draft.to_string(), "Draft");
        assert_eq!(WorkItemStatus::InProgress.to_string(), "InProgress");
        assert_eq!(WorkItemStatus::Abandoned.to_string(), "Abandoned");
    }

    #[test]
    fn test_work_item_status_serde_roundtrip() {
        let status = WorkItemStatus::InReview;
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: WorkItemStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, deserialized);
    }

    #[test]
    fn test_work_item_status_display_matches_serde() {
        // Regression: Display must produce values that serde can deserialize.
        for status in [
            WorkItemStatus::Draft,
            WorkItemStatus::Ready,
            WorkItemStatus::InProgress,
            WorkItemStatus::Blocked,
            WorkItemStatus::InReview,
            WorkItemStatus::Integrated,
            WorkItemStatus::Done,
            WorkItemStatus::Abandoned,
        ] {
            let display = status.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: WorkItemStatus = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("Display output '{}' not deserializable: {}", display, e));
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_work_item_new() {
        let wi = WorkItem::new(
            "phase-123".to_string(),
            "Implement JWT".to_string(),
            "Add JWT signing".to_string(),
        );
        assert_eq!(wi.phase_id, "phase-123");
        assert_eq!(wi.title, "Implement JWT");
        assert_eq!(wi.description, "Add JWT signing");
        assert_eq!(wi.status, WorkItemStatus::Draft);
        assert!(wi.assignee.is_none());
        assert!(wi.resource_tags.is_empty());
        assert!(wi.dependencies.is_empty());
        assert!(!wi.id.is_empty());
        assert!(wi.created_at > 0);
        assert_eq!(wi.created_at, wi.updated_at);
    }

    #[test]
    fn test_work_item_serde_roundtrip() {
        let mut wi = WorkItem::new(
            "phase-456".to_string(),
            "Test WI".to_string(),
            "Description".to_string(),
        );
        wi.assignee = Some("alice".to_string());
        wi.resource_tags = vec!["src/auth.rs".to_string()];
        wi.dependencies = vec!["wi-001".to_string()];

        let json = serde_json::to_string(&wi).unwrap();
        let deserialized: WorkItem = serde_json::from_str(&json).unwrap();
        assert_eq!(wi.id, deserialized.id);
        assert_eq!(wi.phase_id, deserialized.phase_id);
        assert_eq!(wi.assignee, deserialized.assignee);
        assert_eq!(wi.resource_tags, deserialized.resource_tags);
        assert_eq!(wi.dependencies, deserialized.dependencies);
        assert_eq!(wi.status, deserialized.status);
    }

    #[test]
    fn test_work_item_unique_ids() {
        let w1 = WorkItem::new("p".to_string(), "A".to_string(), "".to_string());
        let w2 = WorkItem::new("p".to_string(), "B".to_string(), "".to_string());
        assert_ne!(w1.id, w2.id);
    }

    // --- Valid transitions ---

    #[test]
    fn test_valid_draft_to_ready() {
        let rules = work_item_transitions();
        assert!(validate_transition(WorkItemStatus::Draft, WorkItemStatus::Ready, Role::Coordinator, &rules,).is_ok());
    }

    #[test]
    fn test_valid_ready_to_in_progress() {
        let rules = work_item_transitions();
        assert!(
            validate_transition(
                WorkItemStatus::Ready,
                WorkItemStatus::InProgress,
                Role::Coordinator,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_valid_in_progress_to_blocked_any_role() {
        let rules = work_item_transitions();
        // InProgress → Blocked has role: None (any role)
        for role in [Role::Coordinator, Role::Integrator, Role::Implementer] {
            assert!(
                validate_transition(WorkItemStatus::InProgress, WorkItemStatus::Blocked, role, &rules,).is_ok(),
                "Expected InProgress→Blocked to succeed for {:?}",
                role
            );
        }
    }

    #[test]
    fn test_valid_blocked_to_ready() {
        let rules = work_item_transitions();
        assert!(
            validate_transition(
                WorkItemStatus::Blocked,
                WorkItemStatus::Ready,
                Role::Coordinator,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_valid_in_progress_to_in_review() {
        let rules = work_item_transitions();
        assert!(
            validate_transition(
                WorkItemStatus::InProgress,
                WorkItemStatus::InReview,
                Role::Implementer,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_valid_in_review_to_in_progress_rejection() {
        let rules = work_item_transitions();
        assert!(
            validate_transition(
                WorkItemStatus::InReview,
                WorkItemStatus::InProgress,
                Role::Coordinator,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_valid_in_review_to_integrated() {
        let rules = work_item_transitions();
        assert!(
            validate_transition(
                WorkItemStatus::InReview,
                WorkItemStatus::Integrated,
                Role::Integrator,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_valid_integrated_to_done() {
        let rules = work_item_transitions();
        assert!(
            validate_transition(
                WorkItemStatus::Integrated,
                WorkItemStatus::Done,
                Role::Coordinator,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_valid_abandoned_from_all_non_terminal() {
        let rules = work_item_transitions();
        let non_terminal = [
            WorkItemStatus::Draft,
            WorkItemStatus::Ready,
            WorkItemStatus::InProgress,
            WorkItemStatus::Blocked,
            WorkItemStatus::InReview,
            WorkItemStatus::Integrated,
        ];
        for from in non_terminal {
            assert!(
                validate_transition(from, WorkItemStatus::Abandoned, Role::Coordinator, &rules,).is_ok(),
                "Expected {:?}→Abandoned to succeed",
                from
            );
        }
    }

    // --- Invalid transitions ---

    #[test]
    fn test_invalid_draft_to_ready_wrong_role() {
        let rules = work_item_transitions();
        assert!(validate_transition(WorkItemStatus::Draft, WorkItemStatus::Ready, Role::Implementer, &rules,).is_err());
    }

    #[test]
    fn test_invalid_skip_draft_to_in_progress() {
        let rules = work_item_transitions();
        assert!(
            validate_transition(
                WorkItemStatus::Draft,
                WorkItemStatus::InProgress,
                Role::Coordinator,
                &rules,
            )
            .is_err()
        );
    }

    #[test]
    fn test_invalid_done_to_anything() {
        let rules = work_item_transitions();
        // Done is terminal — no transitions out
        for target in [WorkItemStatus::Draft, WorkItemStatus::Ready, WorkItemStatus::InProgress] {
            assert!(
                validate_transition(WorkItemStatus::Done, target, Role::Coordinator, &rules,).is_err(),
                "Expected Done→{:?} to fail",
                target
            );
        }
    }

    #[test]
    fn test_invalid_abandoned_to_anything() {
        let rules = work_item_transitions();
        // Abandoned is terminal — no transitions out
        assert!(
            validate_transition(
                WorkItemStatus::Abandoned,
                WorkItemStatus::Draft,
                Role::Coordinator,
                &rules,
            )
            .is_err()
        );
    }

    #[test]
    fn test_invalid_in_review_to_integrated_wrong_role() {
        let rules = work_item_transitions();
        // Only Integrator can move InReview → Integrated
        assert!(
            validate_transition(
                WorkItemStatus::InReview,
                WorkItemStatus::Integrated,
                Role::Coordinator,
                &rules,
            )
            .is_err()
        );
    }

    #[test]
    fn test_invalid_in_progress_to_in_review_wrong_role() {
        let rules = work_item_transitions();
        // Only Implementer can move InProgress → InReview
        assert!(
            validate_transition(
                WorkItemStatus::InProgress,
                WorkItemStatus::InReview,
                Role::Coordinator,
                &rules,
            )
            .is_err()
        );
    }

    #[test]
    fn test_invalid_abandoned_not_by_implementer() {
        let rules = work_item_transitions();
        // Abandoned requires Coordinator
        assert!(
            validate_transition(
                WorkItemStatus::Ready,
                WorkItemStatus::Abandoned,
                Role::Implementer,
                &rules,
            )
            .is_err()
        );
    }

    // --- Record trait tests ---

    #[test]
    fn test_record_id() {
        let wi = WorkItem::new("phase-1".into(), "Title".into(), "Desc".into());
        assert_eq!(Record::id(&wi), wi.id);
    }

    #[test]
    fn test_record_updated_at() {
        let wi = WorkItem::new("phase-1".into(), "Title".into(), "Desc".into());
        assert_eq!(Record::updated_at(&wi), wi.updated_at);
    }

    #[test]
    fn test_record_collection_name() {
        assert_eq!(WorkItem::collection_name(), "work_items");
    }

    #[test]
    fn test_record_indexed_fields_status() {
        let wi = WorkItem::new("phase-1".into(), "Title".into(), "Desc".into());
        let fields = wi.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Draft".to_string())));
    }

    #[test]
    fn test_record_indexed_fields_phase_id() {
        let wi = WorkItem::new("phase-abc".into(), "Title".into(), "Desc".into());
        let fields = wi.indexed_fields();
        assert_eq!(
            fields.get("phase_id"),
            Some(&IndexValue::String("phase-abc".to_string()))
        );
    }
}
