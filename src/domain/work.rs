use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use loopr_derive::{FlexibleEnum, Fsm};

use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::markdown::{DocMarkdown, FmValue, millis_to_iso, strip_markdown_section};
use crate::id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, FlexibleEnum, Fsm)]
pub enum WorkStatus {
    #[transitions(Ready(Coordinator), Abandoned(Coordinator))]
    Draft,
    #[transitions(
        InProgress(Coordinator),
        Blocked(Coordinator),
        Abandoned(Coordinator),
        Done(Coordinator)
    )]
    Ready,
    #[transitions(Blocked, InReview(Implementer), Abandoned(Coordinator))]
    #[overrides(Ready(Coordinator), InReview(Coordinator))]
    InProgress,
    #[transitions(Ready(Coordinator), Abandoned(Coordinator))]
    Blocked,
    #[transitions(InProgress(Coordinator), Integrated(Integrator), Abandoned(Coordinator))]
    #[overrides(Ready(Coordinator))]
    InReview,
    #[transitions(Done(Coordinator, Integrator), Abandoned(Coordinator))]
    Integrated,
    Done,
    Abandoned,
}

impl std::fmt::Display for WorkStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// A single item in a Work's completion checklist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecklistItem {
    pub description: String,
    pub completed: bool,
}

/// Concrete unit of work within a Phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub description: String,
    pub assignee: Option<String>,
    status: WorkStatus,
    pub resource_tags: Vec<String>,
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    #[serde(default)]
    pub checklist: Vec<ChecklistItem>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Work {
    /// Read current status.
    pub fn status(&self) -> WorkStatus {
        self.status
    }

    /// Validated FSM transition. Returns Err if invalid.
    pub fn transition(
        &mut self,
        target: WorkStatus,
        role: crate::domain::role::Role,
    ) -> crate::error::Result<crate::domain::transition::Transition> {
        let result = self.status.validate_transition(target, role)?;
        if result == crate::domain::transition::Transition::Changed {
            self.status = target;
            self.updated_at = id::now_millis();
        }
        Ok(result)
    }

    /// Validated FSM override transition (Work has override edges).
    pub fn transition_override(
        &mut self,
        target: WorkStatus,
        role: crate::domain::role::Role,
    ) -> crate::error::Result<crate::domain::transition::Transition> {
        let result = self.status.validate_override(target, role)?;
        if result == crate::domain::transition::Transition::Changed {
            self.status = target;
            self.updated_at = id::now_millis();
        }
        Ok(result)
    }

    /// Bypass FSM validation. For recovery, bootstrap, and test fixtures ONLY.
    pub fn force_status(&mut self, target: WorkStatus) {
        self.status = target;
        self.updated_at = id::now_millis();
    }

    pub fn new(parent_id: String, title: String, description: String) -> Self {
        tracing::debug!("Work::new(parent_id={}, title={})", parent_id, title);
        let now = id::now_millis();
        Self {
            id: id::generate_id("wk"),
            parent_id,
            title,
            description,
            assignee: None,
            status: WorkStatus::Draft,
            resource_tags: Vec::new(),
            dependencies: Vec::new(),
            acceptance_criteria: AcceptanceCriteria::default(),
            checklist: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

impl DocMarkdown for Work {
    fn doc_id(&self) -> &str {
        &self.id
    }

    fn doc_body(&self) -> String {
        let mut body = strip_markdown_section(&self.description, "Acceptance Criteria");
        if !self.acceptance_criteria.is_empty() {
            body.push_str("\n\n## Acceptance Criteria\n\n");
            for item in &self.acceptance_criteria.0 {
                body.push_str(&format!("- [ ] {}\n", item));
            }
        }
        if !self.checklist.is_empty() {
            body.push_str("\n\n## Checklist\n\n");
            for item in &self.checklist {
                let mark = if item.completed { "x" } else { " " };
                body.push_str(&format!("- [{}] {}\n", mark, item.description));
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
        if let Some(ref assignee) = self.assignee {
            m.push(("assignee".into(), FmValue::Text(assignee.clone())));
        }
        m.push(("resource-tags".into(), FmValue::List(self.resource_tags.clone())));
        m.push(("dependencies".into(), FmValue::List(self.dependencies.clone())));
        m.push((
            "acceptance-criteria".into(),
            FmValue::List(self.acceptance_criteria.0.clone()),
        ));
        m.push(("created-at".into(), FmValue::Text(millis_to_iso(self.created_at))));
        m.push(("updated-at".into(), FmValue::Text(millis_to_iso(self.updated_at))));
        m
    }
}

impl Record for Work {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "works"
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

    #[test]
    fn test_work_status_display() {
        assert_eq!(WorkStatus::Draft.to_string(), "Draft");
        assert_eq!(WorkStatus::InProgress.to_string(), "InProgress");
        assert_eq!(WorkStatus::Abandoned.to_string(), "Abandoned");
    }

    #[test]
    fn test_work_status_serde_roundtrip() {
        let status = WorkStatus::InReview;
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: WorkStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, deserialized);
    }

    #[test]
    fn test_work_status_display_matches_serde() {
        for status in [
            WorkStatus::Draft,
            WorkStatus::Ready,
            WorkStatus::InProgress,
            WorkStatus::Blocked,
            WorkStatus::InReview,
            WorkStatus::Integrated,
            WorkStatus::Done,
            WorkStatus::Abandoned,
        ] {
            let display = status.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: WorkStatus = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("Display output '{}' not deserializable: {}", display, e));
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_work_new() {
        let wi = Work::new(
            "phase-123".to_string(),
            "Implement JWT".to_string(),
            "Add JWT signing".to_string(),
        );
        assert_eq!(wi.parent_id, "phase-123");
        assert_eq!(wi.title, "Implement JWT");
        assert_eq!(wi.description, "Add JWT signing");
        assert_eq!(wi.status(), WorkStatus::Draft);
        assert!(wi.assignee.is_none());
        assert!(wi.resource_tags.is_empty());
        assert!(wi.dependencies.is_empty());
        assert!(!wi.id.is_empty());
        assert!(wi.created_at > 0);
        assert_eq!(wi.created_at, wi.updated_at);
    }

    #[test]
    fn test_work_serde_roundtrip() {
        let mut wi = Work::new(
            "phase-456".to_string(),
            "Test WI".to_string(),
            "Description".to_string(),
        );
        wi.assignee = Some("alice".to_string());
        wi.resource_tags = vec!["src/auth.rs".to_string()];
        wi.dependencies = vec!["wi-001".to_string()];

        let json = serde_json::to_string(&wi).unwrap();
        let deserialized: Work = serde_json::from_str(&json).unwrap();
        assert_eq!(wi.id, deserialized.id);
        assert_eq!(wi.parent_id, deserialized.parent_id);
        assert_eq!(wi.assignee, deserialized.assignee);
        assert_eq!(wi.resource_tags, deserialized.resource_tags);
        assert_eq!(wi.dependencies, deserialized.dependencies);
        assert_eq!(wi.status(), deserialized.status());
    }

    #[test]
    fn test_work_unique_ids() {
        let w1 = Work::new("p".to_string(), "A".to_string(), "".to_string());
        let w2 = Work::new("p".to_string(), "B".to_string(), "".to_string());
        assert_ne!(w1.id, w2.id);
    }

    // --- Valid transitions (derived via #[derive(Fsm)]) ---

    #[test]
    fn test_valid_draft_to_ready() {
        assert!(
            WorkStatus::Draft
                .validate_transition(WorkStatus::Ready, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_ready_to_in_progress() {
        assert!(
            WorkStatus::Ready
                .validate_transition(WorkStatus::InProgress, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_ready_to_done_coordinator() {
        assert!(
            WorkStatus::Ready
                .validate_transition(WorkStatus::Done, Role::Coordinator)
                .is_ok(),
            "Coordinator should be able to short-circuit Ready->Done for pre-flight AC pass"
        );
    }

    #[test]
    fn test_invalid_ready_to_done_wrong_role() {
        assert!(
            WorkStatus::Ready
                .validate_transition(WorkStatus::Done, Role::Implementer)
                .is_err(),
            "Only Coordinator can short-circuit Ready->Done"
        );
    }

    #[test]
    fn test_valid_in_progress_to_blocked_any_role() {
        for role in [Role::Coordinator, Role::Integrator, Role::Implementer] {
            assert!(
                WorkStatus::InProgress
                    .validate_transition(WorkStatus::Blocked, role)
                    .is_ok(),
                "Expected InProgress->Blocked to succeed for {:?}",
                role
            );
        }
    }

    #[test]
    fn test_valid_blocked_to_ready() {
        assert!(
            WorkStatus::Blocked
                .validate_transition(WorkStatus::Ready, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_in_progress_to_in_review() {
        assert!(
            WorkStatus::InProgress
                .validate_transition(WorkStatus::InReview, Role::Implementer)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_in_review_to_in_progress_rejection() {
        assert!(
            WorkStatus::InReview
                .validate_transition(WorkStatus::InProgress, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_in_review_to_integrated() {
        assert!(
            WorkStatus::InReview
                .validate_transition(WorkStatus::Integrated, Role::Integrator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_integrated_to_done() {
        assert!(
            WorkStatus::Integrated
                .validate_transition(WorkStatus::Done, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_integrated_to_done_integrator() {
        assert!(
            WorkStatus::Integrated
                .validate_transition(WorkStatus::Done, Role::Integrator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_abandoned_from_all_non_terminal() {
        let non_terminal = [
            WorkStatus::Draft,
            WorkStatus::Ready,
            WorkStatus::InProgress,
            WorkStatus::Blocked,
            WorkStatus::InReview,
            WorkStatus::Integrated,
        ];
        for from in non_terminal {
            assert!(
                from.validate_transition(WorkStatus::Abandoned, Role::Coordinator)
                    .is_ok(),
                "Expected {:?}->Abandoned to succeed",
                from
            );
        }
    }

    // --- Invalid transitions ---

    #[test]
    fn test_invalid_draft_to_ready_wrong_role() {
        assert!(
            WorkStatus::Draft
                .validate_transition(WorkStatus::Ready, Role::Implementer)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_skip_draft_to_in_progress() {
        assert!(
            WorkStatus::Draft
                .validate_transition(WorkStatus::InProgress, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_done_to_anything() {
        for target in [WorkStatus::Draft, WorkStatus::Ready, WorkStatus::InProgress] {
            assert!(
                WorkStatus::Done.validate_transition(target, Role::Coordinator).is_err(),
                "Expected Done->{:?} to fail",
                target
            );
        }
    }

    #[test]
    fn test_invalid_abandoned_to_anything() {
        assert!(
            WorkStatus::Abandoned
                .validate_transition(WorkStatus::Draft, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_in_review_to_integrated_wrong_role() {
        assert!(
            WorkStatus::InReview
                .validate_transition(WorkStatus::Integrated, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_in_progress_to_in_review_wrong_role() {
        assert!(
            WorkStatus::InProgress
                .validate_transition(WorkStatus::InReview, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_abandoned_not_by_implementer() {
        assert!(
            WorkStatus::Ready
                .validate_transition(WorkStatus::Abandoned, Role::Implementer)
                .is_err()
        );
    }

    // --- Record trait tests ---

    #[test]
    fn test_record_id() {
        let wi = Work::new("phase-1".into(), "Title".into(), "Desc".into());
        assert_eq!(Record::id(&wi), wi.id);
    }

    #[test]
    fn test_record_updated_at() {
        let wi = Work::new("phase-1".into(), "Title".into(), "Desc".into());
        assert_eq!(Record::updated_at(&wi), wi.updated_at);
    }

    #[test]
    fn test_record_collection_name() {
        assert_eq!(Work::collection_name(), "works");
    }

    #[test]
    fn test_record_indexed_fields_status() {
        let wi = Work::new("phase-1".into(), "Title".into(), "Desc".into());
        let fields = wi.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Draft".to_string())));
    }

    #[test]
    fn test_record_indexed_fields_parent_id() {
        let wi = Work::new("phase-abc".into(), "Title".into(), "Desc".into());
        let fields = wi.indexed_fields();
        assert_eq!(
            fields.get("parent_id"),
            Some(&IndexValue::String("phase-abc".to_string()))
        );
    }

    // --- Override transition tests (validate_override includes normal + override edges) ---

    #[test]
    fn test_override_in_progress_to_ready_coordinator() {
        assert!(
            WorkStatus::InProgress
                .validate_override(WorkStatus::Ready, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_override_in_progress_to_in_review_coordinator() {
        assert!(
            WorkStatus::InProgress
                .validate_override(WorkStatus::InReview, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_override_in_progress_to_abandoned_coordinator() {
        assert!(
            WorkStatus::InProgress
                .validate_override(WorkStatus::Abandoned, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_override_in_review_to_ready_coordinator() {
        assert!(
            WorkStatus::InReview
                .validate_override(WorkStatus::Ready, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_override_in_review_to_abandoned_coordinator() {
        assert!(
            WorkStatus::InReview
                .validate_override(WorkStatus::Abandoned, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_override_blocked_to_abandoned_coordinator() {
        assert!(
            WorkStatus::Blocked
                .validate_override(WorkStatus::Abandoned, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_override_rejected_for_implementer() {
        assert!(
            WorkStatus::InProgress
                .validate_override(WorkStatus::Ready, Role::Implementer)
                .is_err()
        );
    }

    #[test]
    fn test_override_rejected_for_integrator() {
        assert!(
            WorkStatus::InProgress
                .validate_override(WorkStatus::Ready, Role::Integrator)
                .is_err()
        );
    }

    #[test]
    fn test_override_not_in_normal_transitions() {
        assert!(
            WorkStatus::InProgress
                .validate_transition(WorkStatus::Ready, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_override_in_review_to_ready_not_in_normal() {
        assert!(
            WorkStatus::InReview
                .validate_transition(WorkStatus::Ready, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_is_terminal() {
        assert!(!WorkStatus::Draft.is_terminal());
        assert!(!WorkStatus::InProgress.is_terminal());
        assert!(WorkStatus::Done.is_terminal());
        assert!(WorkStatus::Abandoned.is_terminal());
    }

    #[test]
    fn test_idempotent_self_transition() {
        let r = WorkStatus::Draft.validate_transition(WorkStatus::Draft, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Unchanged);
    }

    // --- FlexibleEnum tests ---

    #[test]
    fn test_flexible_enum_lowercase() {
        assert_eq!("draft".parse::<WorkStatus>().unwrap(), WorkStatus::Draft);
        assert_eq!("ready".parse::<WorkStatus>().unwrap(), WorkStatus::Ready);
        assert_eq!("inprogress".parse::<WorkStatus>().unwrap(), WorkStatus::InProgress);
        assert_eq!("blocked".parse::<WorkStatus>().unwrap(), WorkStatus::Blocked);
        assert_eq!("inreview".parse::<WorkStatus>().unwrap(), WorkStatus::InReview);
        assert_eq!("integrated".parse::<WorkStatus>().unwrap(), WorkStatus::Integrated);
        assert_eq!("done".parse::<WorkStatus>().unwrap(), WorkStatus::Done);
        assert_eq!("abandoned".parse::<WorkStatus>().unwrap(), WorkStatus::Abandoned);
    }

    #[test]
    fn test_flexible_enum_pascal_case() {
        assert_eq!("Draft".parse::<WorkStatus>().unwrap(), WorkStatus::Draft);
        assert_eq!("Ready".parse::<WorkStatus>().unwrap(), WorkStatus::Ready);
        assert_eq!("InProgress".parse::<WorkStatus>().unwrap(), WorkStatus::InProgress);
    }

    #[test]
    fn test_flexible_enum_uppercase() {
        assert_eq!("DRAFT".parse::<WorkStatus>().unwrap(), WorkStatus::Draft);
        assert_eq!("READY".parse::<WorkStatus>().unwrap(), WorkStatus::Ready);
        assert_eq!("INPROGRESS".parse::<WorkStatus>().unwrap(), WorkStatus::InProgress);
    }

    #[test]
    fn test_flexible_enum_rejects_underscores() {
        let err = "in_progress".parse::<WorkStatus>().unwrap_err();
        assert!(err.contains("underscores and hyphens are not allowed"));
        assert!(err.contains("valid:"));
    }

    #[test]
    fn test_flexible_enum_rejects_hyphens() {
        let err = "in-progress".parse::<WorkStatus>().unwrap_err();
        assert!(err.contains("underscores and hyphens are not allowed"));
    }

    #[test]
    fn test_flexible_enum_invalid_value() {
        let err = "bogus".parse::<WorkStatus>().unwrap_err();
        assert!(err.contains("invalid WorkStatus"));
        assert!(err.contains("Draft"));
        assert!(err.contains("Ready"));
    }

    #[test]
    fn test_flexible_enum_variant_names() {
        assert_eq!(
            WorkStatus::VARIANT_NAMES,
            &[
                "Draft",
                "Ready",
                "InProgress",
                "Blocked",
                "InReview",
                "Integrated",
                "Done",
                "Abandoned"
            ]
        );
    }
}
