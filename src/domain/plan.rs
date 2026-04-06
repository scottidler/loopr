use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use taskstore::record::{IndexValue, Record};

use loopr_derive::{FlexibleEnum, Fsm};

use crate::id;

/// Shared status enum for Plan, Spec, and Phase records.
/// All three use the same four-state machine with Coordinator-only transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, FlexibleEnum, Fsm)]
#[serde(rename_all = "lowercase")]
pub enum HierarchyStatus {
    #[serde(alias = "Draft")]
    #[transitions(Active(Coordinator), Abandoned(Coordinator))]
    Draft,
    #[serde(alias = "Active")]
    #[transitions(Complete(Coordinator), Abandoned(Coordinator))]
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

/// Classify a Plan as Full or Brief using the tier-gate prompt and an LLM call.
///
/// Reads the tier-gate.pmt prompt, appends the Plan's description, calls the LLM,
/// and parses "full" or "brief" from the response. Falls back to Full on any error.
///
/// Uses Haiku (or whatever model the ValidatorConfig specifies) - this is a binary
/// classification task, not a reasoning task.
pub async fn classify_tier(plan: &Plan, client: &crate::validator::client::LlmClient) -> Tier {
    let prompt_template = crate::prompts::store().tier_gate.clone();
    if prompt_template.is_empty() {
        tracing::warn!("tier-gate prompt is empty, defaulting to Full");
        return Tier::Full;
    }

    let prompt = format!("{}\n{}", prompt_template, plan.description);

    match client.call(&prompt).await {
        Ok(response) => match parse_tier(&response) {
            Some(tier) => {
                tracing::info!("Tier classification: {:?} (plan={})", tier, plan.id);
                tier
            }
            None => {
                tracing::warn!(
                    "Tier classification returned {:?}, retrying (plan={})",
                    response.trim(),
                    plan.id
                );
                retry_tier(client, &response, &plan.id).await
            }
        },
        Err(e) => {
            tracing::warn!("Tier classification failed, defaulting to Full: {}", e);
            Tier::Full
        }
    }
}

fn parse_tier(response: &str) -> Option<Tier> {
    match response.trim().to_lowercase().as_str() {
        "brief" => Some(Tier::Brief),
        "full" => Some(Tier::Full),
        _ => None,
    }
}

async fn retry_tier(client: &crate::validator::client::LlmClient, bad_response: &str, plan_id: &str) -> Tier {
    let correction = format!(
        "You responded with {:?} but the only valid responses are \
         exactly \"Brief\" or \"Full\". Reply with one of those two \
         words only, nothing else.",
        bad_response.trim()
    );
    match client.call(&correction).await {
        Ok(response) => match parse_tier(&response) {
            Some(tier) => {
                tracing::info!("Tier classification on retry: {:?} (plan={})", tier, plan_id);
                tier
            }
            None => {
                tracing::error!(
                    "Tier classification retry also returned {:?}, \
                     defaulting to Full (plan={})",
                    response.trim(),
                    plan_id
                );
                Tier::Full
            }
        },
        Err(e) => {
            tracing::error!("Tier classification retry failed, defaulting to Full: {}", e);
            Tier::Full
        }
    }
}

/// Top-level objective. Contains markdown description and acceptance criteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: String,
    status: PlanStatus,
    #[serde(default)]
    pub tier: Tier,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Plan {
    /// Read current status.
    pub fn status(&self) -> PlanStatus {
        self.status
    }

    /// Validated FSM transition. Returns Err if invalid.
    pub fn transition(
        &mut self,
        target: PlanStatus,
        role: crate::domain::role::Role,
    ) -> crate::error::Result<crate::domain::transition::Transition> {
        let result = self.status.validate_transition(target, role)?;
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

    pub fn new(title: String, description: String, acceptance_criteria: String) -> Self {
        tracing::debug!("Plan::new(title={})", title);
        let now = id::now_millis();
        Self {
            id: id::generate_id("pl"),
            title,
            description,
            acceptance_criteria,
            status: HierarchyStatus::Draft,
            tier: Tier::default(),
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
        m.insert("tier".into(), IndexValue::String(self.tier.to_string()));
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::role::Role;
    use crate::domain::transition::Transition;

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

    // --- HierarchyStatus transition tests (derived via #[derive(Fsm)]) ---

    #[test]
    fn test_valid_transition_draft_to_active() {
        let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Active, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Changed);
    }

    #[test]
    fn test_valid_transition_active_to_complete() {
        let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Complete, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Changed);
    }

    #[test]
    fn test_valid_transition_draft_to_abandoned() {
        let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Abandoned, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Changed);
    }

    #[test]
    fn test_valid_transition_active_to_abandoned() {
        let r = HierarchyStatus::Active.validate_transition(HierarchyStatus::Abandoned, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Changed);
    }

    #[test]
    fn test_invalid_transition_complete_to_active() {
        assert!(
            HierarchyStatus::Complete
                .validate_transition(HierarchyStatus::Active, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_transition_abandoned_to_active() {
        assert!(
            HierarchyStatus::Abandoned
                .validate_transition(HierarchyStatus::Active, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_transition_wrong_role() {
        assert!(
            HierarchyStatus::Draft
                .validate_transition(HierarchyStatus::Active, Role::Implementer)
                .is_err()
        );
        assert!(
            HierarchyStatus::Draft
                .validate_transition(HierarchyStatus::Active, Role::Integrator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_transition_draft_to_complete() {
        assert!(
            HierarchyStatus::Draft
                .validate_transition(HierarchyStatus::Complete, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_is_terminal() {
        assert!(!HierarchyStatus::Draft.is_terminal());
        assert!(!HierarchyStatus::Active.is_terminal());
        assert!(HierarchyStatus::Complete.is_terminal());
        assert!(HierarchyStatus::Abandoned.is_terminal());
    }

    #[test]
    fn test_idempotent_self_transition() {
        let r = HierarchyStatus::Draft.validate_transition(HierarchyStatus::Draft, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Unchanged);
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
        assert_eq!(plan.status(), HierarchyStatus::Draft);
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
        assert_eq!(plan.status(), deserialized.status());
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
        let plan = Plan::new("RT".to_string(), "Desc".to_string(), "Crit".to_string());
        let json = serde_json::to_string(&plan).unwrap();
        let restored: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(Record::id(&restored), Record::id(&plan));
        assert_eq!(Record::updated_at(&restored), Record::updated_at(&plan));
        assert_eq!(Plan::collection_name(), "plans");
    }
}
