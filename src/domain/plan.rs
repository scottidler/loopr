use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use taskstore::record::{IndexValue, Record};

use loopr_derive::FlexibleEnum;

use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::markdown::{DocMarkdown, FmValue, millis_to_iso};
use crate::id;
use crate::prompts::SECTION_AC;

/// Shared status enum for Plan, Spec, and Phase records.
/// Six-state machine: Draft -> Pending -> Active -> Complete | Superseded | Abandoned.
/// Pending is the "waiting for deps + parent" state introduced by the reactive execution model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, FlexibleEnum)]
#[serde(rename_all = "lowercase")]
pub enum HierarchyStatus {
    #[serde(alias = "Draft")]
    Draft,
    #[serde(alias = "Pending")]
    Pending,
    #[serde(alias = "Active")]
    Active,
    #[serde(alias = "Complete")]
    Complete,
    #[serde(alias = "Superseded")]
    Superseded,
    #[serde(alias = "Abandoned")]
    Abandoned,
}

impl fmt::Display for HierarchyStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HierarchyStatus::Draft => write!(f, "draft"),
            HierarchyStatus::Pending => write!(f, "pending"),
            HierarchyStatus::Active => write!(f, "active"),
            HierarchyStatus::Complete => write!(f, "complete"),
            HierarchyStatus::Superseded => write!(f, "superseded"),
            HierarchyStatus::Abandoned => write!(f, "abandoned"),
        }
    }
}

/// Type aliases so each record can name its own status type.
pub type PlanStatus = HierarchyStatus;

/// Lifecycle tier: Full (Plan -> Spec -> Phase -> Work) or Brief (Plan -> Work).
///
/// Determined once when a Plan is activated. Precedence:
///   1. Explicit user override during interview
///   2. LLM classification (Haiku reads Plan, checks for contract definitions)
///   3. Default: Full
///
/// Stored on Plan (persistent). A Plan that defines no contracts is always Brief.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    #[default]
    Full,
    Brief,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tier::Full => write!(f, "full"),
            Tier::Brief => write!(f, "brief"),
        }
    }
}

/// Top-level objective. Contains acceptance criteria and lifecycle status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub acceptance_criteria: AcceptanceCriteria,
    status: PlanStatus,
    #[serde(default)]
    pub tier: Tier,
    /// Number of times decomposition has been attempted for this plan.
    /// Used by decomposition-attempt-limit threshold trigger to prevent infinite retry loops.
    #[serde(default)]
    pub decomposition_attempts: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Plan {
    /// Read current status.
    pub fn status(&self) -> PlanStatus {
        self.status
    }

    /// Validated FSM transition via the runtime interpreter.
    pub fn transition(
        &mut self,
        target: PlanStatus,
        role: crate::domain::role::Role,
        fsm: &crate::fsm::runtime::FsmInterpreter,
    ) -> eyre::Result<crate::domain::transition::Transition> {
        use crate::fsm::status::FsmStatus;
        let result = fsm.validate_transition(
            HierarchyStatus::fsm_name(),
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
    pub fn force_status(&mut self, target: PlanStatus) {
        self.status = target;
        self.updated_at = id::now_millis();
    }

    pub fn new(title: String, acceptance_criteria: AcceptanceCriteria) -> Self {
        tracing::debug!("Plan::new(title={})", title);
        let now = id::now_millis();
        Self {
            id: id::generate_id("pl"),
            parent_id: None,
            title,
            acceptance_criteria,
            status: HierarchyStatus::Draft,
            tier: Tier::default(),
            decomposition_attempts: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

impl DocMarkdown for Plan {
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
        m.push((
            "parent-id".into(),
            FmValue::Text(self.parent_id.as_deref().unwrap_or("~").to_string()),
        ));
        m.push(("title".into(), FmValue::Text(self.title.clone())));
        m.push(("status".into(), FmValue::Text(format!("{:?}", self.status()))));
        m.push(("tier".into(), FmValue::Text(format!("{:?}", self.tier))));
        m.push((
            "acceptance-criteria".into(),
            FmValue::List(self.acceptance_criteria.0.clone()),
        ));
        m.push(("created-at".into(), FmValue::Text(millis_to_iso(self.created_at))));
        m.push(("updated-at".into(), FmValue::Text(millis_to_iso(self.updated_at))));
        m.push(("children".into(), FmValue::List(vec![])));
        m
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
        m.insert("tier".into(), IndexValue::String(self.tier.to_string()));
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // --- HierarchyStatus tests ---

    #[test]
    fn test_hierarchy_status_display() {
        assert_eq!(HierarchyStatus::Draft.to_string(), "draft");
        assert_eq!(HierarchyStatus::Pending.to_string(), "pending");
        assert_eq!(HierarchyStatus::Active.to_string(), "active");
        assert_eq!(HierarchyStatus::Complete.to_string(), "complete");
        assert_eq!(HierarchyStatus::Superseded.to_string(), "superseded");
        assert_eq!(HierarchyStatus::Abandoned.to_string(), "abandoned");
    }

    #[test]
    fn test_hierarchy_status_serde_roundtrip() {
        for status in [
            HierarchyStatus::Draft,
            HierarchyStatus::Pending,
            HierarchyStatus::Active,
            HierarchyStatus::Complete,
            HierarchyStatus::Superseded,
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
        assert_eq!(serde_json::to_string(&HierarchyStatus::Pending).unwrap(), "\"pending\"");
        assert_eq!(serde_json::to_string(&HierarchyStatus::Active).unwrap(), "\"active\"");
        assert_eq!(
            serde_json::to_string(&HierarchyStatus::Complete).unwrap(),
            "\"complete\""
        );
        assert_eq!(
            serde_json::to_string(&HierarchyStatus::Superseded).unwrap(),
            "\"superseded\""
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
            ("\"Pending\"", HierarchyStatus::Pending),
            ("\"Active\"", HierarchyStatus::Active),
            ("\"Complete\"", HierarchyStatus::Complete),
            ("\"Superseded\"", HierarchyStatus::Superseded),
            ("\"Abandoned\"", HierarchyStatus::Abandoned),
        ] {
            let deserialized: HierarchyStatus = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("PascalCase '{}' should deserialize: {}", json, e));
            assert_eq!(deserialized, expected);
        }
    }

    #[test]
    fn test_hierarchy_status_display_matches_serde() {
        for status in [
            HierarchyStatus::Draft,
            HierarchyStatus::Pending,
            HierarchyStatus::Active,
            HierarchyStatus::Complete,
            HierarchyStatus::Superseded,
            HierarchyStatus::Abandoned,
        ] {
            let display = status.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: HierarchyStatus = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("Display output '{}' not deserializable: {}", display, e));
            assert_eq!(status, deserialized);
        }
    }

    // FSM transition validation tests are in src/fsm/tests.rs (runtime interpreter).

    #[test]
    fn test_is_terminal() {
        use crate::fsm::status::FsmStatus;
        let fsm = crate::fsm::runtime::FsmInterpreter::embedded().unwrap();
        assert!(!HierarchyStatus::Draft.is_terminal(&fsm));
        assert!(!HierarchyStatus::Pending.is_terminal(&fsm));
        assert!(!HierarchyStatus::Active.is_terminal(&fsm));
        assert!(HierarchyStatus::Complete.is_terminal(&fsm));
        assert!(HierarchyStatus::Superseded.is_terminal(&fsm));
        assert!(HierarchyStatus::Abandoned.is_terminal(&fsm));
    }

    // --- Plan struct tests ---

    #[test]
    fn test_plan_new() {
        let ac = AcceptanceCriteria(vec!["It works".to_string()]);
        let plan = Plan::new("Test Plan".to_string(), ac);
        assert_eq!(plan.title, "Test Plan");
        assert_eq!(plan.acceptance_criteria.0, vec!["It works".to_string()]);
        assert_eq!(plan.status(), HierarchyStatus::Draft);
        assert!(!plan.id.is_empty());
        assert!(plan.created_at > 0);
        assert_eq!(plan.created_at, plan.updated_at);
    }

    #[test]
    fn test_plan_serde_roundtrip() {
        let plan = Plan::new("Test Plan".to_string(), AcceptanceCriteria::default());
        let json = serde_json::to_string(&plan).unwrap();
        let deserialized: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.id, deserialized.id);
        assert_eq!(plan.title, deserialized.title);
        assert_eq!(plan.status(), deserialized.status());
        assert_eq!(plan.created_at, deserialized.created_at);
    }

    #[test]
    fn test_plan_unique_ids() {
        let p1 = Plan::new("A".to_string(), AcceptanceCriteria::default());
        let p2 = Plan::new("B".to_string(), AcceptanceCriteria::default());
        assert_ne!(p1.id, p2.id);
    }

    // --- Record trait tests ---

    #[test]
    fn test_plan_record_id() {
        let plan = Plan::new("Test".to_string(), AcceptanceCriteria::default());
        assert_eq!(Record::id(&plan), plan.id.as_str());
    }

    #[test]
    fn test_plan_record_updated_at() {
        let plan = Plan::new("Test".to_string(), AcceptanceCriteria::default());
        assert_eq!(Record::updated_at(&plan), plan.updated_at);
    }

    #[test]
    fn test_plan_record_collection_name() {
        assert_eq!(Plan::collection_name(), "plans");
    }

    #[test]
    fn test_plan_record_indexed_fields() {
        let plan = Plan::new("Test".to_string(), AcceptanceCriteria::default());
        let fields = plan.indexed_fields();
        assert_eq!(fields.len(), 2);
        assert_eq!(
            fields.get("status"),
            Some(&taskstore::record::IndexValue::String("draft".to_string()))
        );
        assert_eq!(
            fields.get("tier"),
            Some(&taskstore::record::IndexValue::String("full".to_string()))
        );
    }

    #[test]
    fn test_plan_record_roundtrip_json() {
        let plan = Plan::new("RT".to_string(), AcceptanceCriteria::default());
        let json = serde_json::to_string(&plan).unwrap();
        let restored: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(Record::id(&restored), Record::id(&plan));
        assert_eq!(Record::updated_at(&restored), Record::updated_at(&plan));
        assert_eq!(Plan::collection_name(), "plans");
    }

    // --- DocMarkdown impl tests ---

    #[test]
    fn test_plan_doc_id() {
        let plan = Plan::new("T".into(), AcceptanceCriteria::default());
        assert_eq!(plan.doc_id(), plan.id.as_str());
    }

    #[test]
    fn test_plan_doc_body_no_ac() {
        let plan = Plan::new("T".into(), AcceptanceCriteria::default());
        let body = plan.doc_body();
        assert_eq!(body, "");
    }

    #[test]
    fn test_plan_doc_body_with_ac() {
        let ac = AcceptanceCriteria(vec!["Tests pass".to_string()]);
        let plan = Plan::new("T".into(), ac);
        let body = plan.doc_body();
        assert!(body.contains(&format!("## {}", SECTION_AC)));
        assert!(body.contains("- [ ] Tests pass"));
    }

    #[test]
    fn test_plan_doc_frontmatter_keys() {
        let plan = Plan::new("My Plan".into(), AcceptanceCriteria::default());
        let fm = plan.doc_frontmatter();
        let keys: Vec<&str> = fm.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"id"));
        assert!(keys.contains(&"parent-id"));
        assert!(keys.contains(&"title"));
        assert!(keys.contains(&"status"));
        assert!(keys.contains(&"tier"));
        assert!(keys.contains(&"acceptance-criteria"));
        assert!(keys.contains(&"created-at"));
        assert!(keys.contains(&"updated-at"));
    }

    #[test]
    fn test_plan_doc_frontmatter_id_first() {
        let plan = Plan::new("T".into(), AcceptanceCriteria::default());
        let fm = plan.doc_frontmatter();
        assert_eq!(fm[0].0, "id", "id must be the first frontmatter key");
    }
}
