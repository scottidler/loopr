use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use loopr_derive::FlexibleEnum;

use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::markdown::{DocMarkdown, FmValue, millis_to_iso};
use crate::id;
use crate::prompts::SECTION_AC;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, FlexibleEnum)]
pub enum WorkStatus {
    Draft,
    Pending,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Integrated,
    Done,
    Superseded,
    Abandoned,
}

impl std::fmt::Display for WorkStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Concrete unit of work within a Phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub assignee: Option<String>,
    status: WorkStatus,
    pub dependencies: Vec<String>,
    /// Files in scope for this Work, declared by the Coordinator at creation time.
    /// Used by the Reviewer context builder to inject HEAD contents for schema verification.
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    /// Number of times this Work has been reset to Ready from a non-Draft state.
    /// Used by the work queue to penalize cycling items and as a hard-limit backstop.
    #[serde(default)]
    pub attempt_count: u32,
    /// Number of consecutive agent session failures (crash/cancel before bundle
    /// creation). Independent of max_bundle_rejections. Reset to 0 on successful
    /// session completion.
    #[serde(default)]
    pub session_failure_count: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Work {
    /// Read current status.
    pub fn status(&self) -> WorkStatus {
        self.status
    }

    /// Validated FSM transition via the runtime interpreter.
    pub fn transition(
        &mut self,
        target: WorkStatus,
        role: crate::domain::role::Role,
        fsm: &crate::fsm::runtime::FsmInterpreter,
    ) -> eyre::Result<crate::domain::transition::Transition> {
        use crate::fsm::status::FsmStatus;
        let result = fsm.validate_transition(
            WorkStatus::fsm_name(),
            self.status.to_yaml_name(),
            target.to_yaml_name(),
            &role.to_string(),
        )?;
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
        fsm: &crate::fsm::runtime::FsmInterpreter,
    ) -> eyre::Result<crate::domain::transition::Transition> {
        use crate::fsm::status::FsmStatus;
        let result = fsm.validate_override(
            WorkStatus::fsm_name(),
            self.status.to_yaml_name(),
            target.to_yaml_name(),
            &role.to_string(),
        )?;
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

    pub fn new(parent_id: String, title: String) -> Self {
        tracing::debug!("Work::new(parent_id={}, title={})", parent_id, title);
        let now = id::now_millis();
        Self {
            id: id::generate_id("wk"),
            parent_id,
            title,
            assignee: None,
            status: WorkStatus::Draft,
            dependencies: Vec::new(),
            files: Vec::new(),
            acceptance_criteria: AcceptanceCriteria::default(),
            attempt_count: 0,
            session_failure_count: 0,
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
        let mut body = String::new();
        if !self.acceptance_criteria.is_empty() {
            body.push_str(&format!("## {}\n\n", SECTION_AC));
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
        if let Some(ref assignee) = self.assignee {
            m.push(("assignee".into(), FmValue::Text(assignee.clone())));
        }
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
            WorkStatus::Superseded,
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
        let wi = Work::new("phase-123".to_string(), "Implement JWT".to_string());
        assert_eq!(wi.parent_id, "phase-123");
        assert_eq!(wi.title, "Implement JWT");
        assert_eq!(wi.status(), WorkStatus::Draft);
        assert!(wi.assignee.is_none());
        assert!(wi.dependencies.is_empty());
        assert!(!wi.id.is_empty());
        assert!(wi.created_at > 0);
        assert_eq!(wi.created_at, wi.updated_at);
    }

    #[test]
    fn test_work_serde_roundtrip() {
        let mut wi = Work::new("phase-456".to_string(), "Test WI".to_string());
        wi.assignee = Some("alice".to_string());
        wi.dependencies = vec!["wi-001".to_string()];

        let json = serde_json::to_string(&wi).unwrap();
        let deserialized: Work = serde_json::from_str(&json).unwrap();
        assert_eq!(wi.id, deserialized.id);
        assert_eq!(wi.parent_id, deserialized.parent_id);
        assert_eq!(wi.assignee, deserialized.assignee);
        assert_eq!(wi.dependencies, deserialized.dependencies);
        assert_eq!(wi.status(), deserialized.status());
    }

    #[test]
    fn test_work_serde_backward_compat_ignores_files() {
        // Old JSONL records with a "files" field must still deserialize (serde ignores unknown fields).
        let json = serde_json::json!({
            "id": "wk-test",
            "parent_id": "phase-1",
            "title": "Old Work",
            "status": "Draft",
            "files": ["src/main.rs"],
            "dependencies": [],
            "acceptance_criteria": [],
            "attempt_count": 0,
            "created_at": 1000,
            "updated_at": 1000
        });
        let work: Work = serde_json::from_value(json).unwrap();
        assert_eq!(work.id, "wk-test");
    }

    #[test]
    fn test_work_unique_ids() {
        let w1 = Work::new("p".to_string(), "A".to_string());
        let w2 = Work::new("p".to_string(), "B".to_string());
        assert_ne!(w1.id, w2.id);
    }

    // FSM transition validation tests are in src/fsm/tests.rs (runtime interpreter).

    // --- Record trait tests ---

    #[test]
    fn test_record_id() {
        let wi = Work::new("phase-1".into(), "Title".into());
        assert_eq!(Record::id(&wi), wi.id);
    }

    #[test]
    fn test_record_updated_at() {
        let wi = Work::new("phase-1".into(), "Title".into());
        assert_eq!(Record::updated_at(&wi), wi.updated_at);
    }

    #[test]
    fn test_record_collection_name() {
        assert_eq!(Work::collection_name(), "works");
    }

    #[test]
    fn test_record_indexed_fields_status() {
        let wi = Work::new("phase-1".into(), "Title".into());
        let fields = wi.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Draft".to_string())));
    }

    #[test]
    fn test_record_indexed_fields_parent_id() {
        let wi = Work::new("phase-abc".into(), "Title".into());
        let fields = wi.indexed_fields();
        assert_eq!(
            fields.get("parent_id"),
            Some(&IndexValue::String("phase-abc".to_string()))
        );
    }

    #[test]
    fn test_is_terminal() {
        use crate::fsm::status::FsmStatus;
        let fsm = crate::fsm::runtime::FsmInterpreter::embedded().unwrap();
        assert!(!WorkStatus::Draft.is_terminal(&fsm));
        assert!(!WorkStatus::InProgress.is_terminal(&fsm));
        assert!(WorkStatus::Done.is_terminal(&fsm));
        assert!(WorkStatus::Abandoned.is_terminal(&fsm));
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
        assert_eq!("superseded".parse::<WorkStatus>().unwrap(), WorkStatus::Superseded);
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
                "Pending",
                "Ready",
                "InProgress",
                "Blocked",
                "InReview",
                "Integrated",
                "Done",
                "Superseded",
                "Abandoned"
            ]
        );
    }
}
