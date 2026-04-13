use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use loopr_derive::FlexibleEnum;

use crate::id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, FlexibleEnum)]
pub enum TickStatus {
    Open,
    Sealing,
    Validating,
    Published,
    Failed,
}

impl TickStatus {
    /// True if this state has no outgoing transitions (terminal).
    pub fn is_terminal(self) -> bool {
        matches!(self, TickStatus::Published | TickStatus::Failed)
    }
}

impl std::fmt::Display for TickStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// An immutable integration checkpoint identified by a Git SHA.
///
/// With the integration branch model, a Tick is scoped to a specific Plan's
/// integration branch, not a global snapshot of main.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tick {
    pub id: String,
    pub number: u32,
    /// Plan this Tick belongs to. Empty for legacy ticks created before the
    /// integration branch model.
    #[serde(default)]
    pub plan_id: String,
    pub integration_sha: Option<String>,
    pub bundle_ids: Vec<String>,
    #[serde(default)]
    pub attempted_bundle_ids: Vec<String>,
    pub validation_log: String,
    status: TickStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Tick {
    /// Read current status.
    pub fn status(&self) -> TickStatus {
        self.status
    }

    /// Validated FSM transition via the runtime interpreter.
    pub fn transition(
        &mut self,
        target: TickStatus,
        role: crate::domain::role::Role,
        fsm: &crate::fsm::runtime::FsmInterpreter,
    ) -> eyre::Result<crate::domain::transition::Transition> {
        use crate::fsm::status::FsmStatus;
        let result = fsm.validate_transition(
            TickStatus::fsm_name(),
            self.status.to_yaml_name(),
            target.to_yaml_name(),
            &role.to_string(),
        )?;
        if result == crate::domain::transition::Transition::Changed {
            self.status = target;
            self.updated_at = crate::id::now_millis();
        }
        Ok(result)
    }

    /// Bypass FSM validation. For recovery, bootstrap, and test fixtures ONLY.
    pub fn force_status(&mut self, target: TickStatus) {
        self.status = target;
        self.updated_at = crate::id::now_millis();
    }

    pub fn new(number: u32) -> Self {
        tracing::debug!("Tick::new(number={})", number);
        let now = id::now_millis();
        Self {
            id: id::generate_id("tk"),
            number,
            plan_id: String::new(),
            integration_sha: None,
            bundle_ids: Vec::new(),
            attempted_bundle_ids: Vec::new(),
            validation_log: String::new(),
            status: TickStatus::Open,
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a Tick scoped to a specific Plan's integration branch.
    pub fn for_plan(number: u32, plan_id: &str) -> Self {
        tracing::debug!("Tick::for_plan(number={}, plan_id={})", number, plan_id);
        let mut tick = Self::new(number);
        tick.plan_id = plan_id.to_string();
        tick
    }
}

impl Record for Tick {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "ticks"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m.insert("number".into(), IndexValue::Int(self.number as i64));
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tick_status_is_terminal() {
        assert!(!TickStatus::Open.is_terminal());
        assert!(!TickStatus::Sealing.is_terminal());
        assert!(!TickStatus::Validating.is_terminal());
        assert!(TickStatus::Published.is_terminal());
        assert!(TickStatus::Failed.is_terminal());
    }

    #[test]
    fn test_tick_status_display() {
        assert_eq!(TickStatus::Open.to_string(), "Open");
        assert_eq!(TickStatus::Sealing.to_string(), "Sealing");
        assert_eq!(TickStatus::Validating.to_string(), "Validating");
        assert_eq!(TickStatus::Published.to_string(), "Published");
        assert_eq!(TickStatus::Failed.to_string(), "Failed");
    }

    #[test]
    fn test_tick_status_serde_roundtrip() {
        let status = TickStatus::Published;
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: TickStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, deserialized);
    }

    #[test]
    fn test_tick_status_serde_format() {
        let json = serde_json::to_string(&TickStatus::Validating).unwrap();
        assert_eq!(json, "\"Validating\"");
    }

    #[test]
    fn test_tick_status_display_matches_serde() {
        // Regression: Display must produce values that serde can deserialize.
        for status in [
            TickStatus::Open,
            TickStatus::Sealing,
            TickStatus::Validating,
            TickStatus::Published,
            TickStatus::Failed,
        ] {
            let display = status.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: TickStatus = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("Display output '{}' not deserializable: {}", display, e));
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_tick_new() {
        let t = Tick::new(1);
        assert_eq!(t.number, 1);
        assert!(t.plan_id.is_empty());
        assert!(t.integration_sha.is_none());
        assert!(t.bundle_ids.is_empty());
        assert!(t.validation_log.is_empty());
        assert_eq!(t.status(), TickStatus::Open);
        assert!(!t.id.is_empty());
        assert!(t.created_at > 0);
        assert_eq!(t.created_at, t.updated_at);
    }

    #[test]
    fn test_tick_for_plan() {
        let t = Tick::for_plan(3, "pl-abc12");
        assert_eq!(t.number, 3);
        assert_eq!(t.plan_id, "pl-abc12");
        assert_eq!(t.status(), TickStatus::Open);
    }

    #[test]
    fn test_tick_serde_backward_compat_without_plan_id() {
        // Old JSONL records without plan_id should deserialize with empty string (serde default).
        let json = serde_json::json!({
            "id": "tk-test",
            "number": 1,
            "integration_sha": null,
            "bundle_ids": [],
            "validation_log": "",
            "status": "Open",
            "created_at": 1000,
            "updated_at": 1000
        });
        let tick: Tick = serde_json::from_value(json).unwrap();
        assert!(tick.plan_id.is_empty());
    }

    #[test]
    fn test_tick_serde_roundtrip() {
        let mut t = Tick::new(5);
        t.bundle_ids = vec!["bundle-001".to_string(), "bundle-002".to_string()];
        t.validation_log = "all tests passed".to_string();
        t.integration_sha = Some("abc123def456".to_string());

        let json = serde_json::to_string(&t).unwrap();
        let deserialized: Tick = serde_json::from_str(&json).unwrap();
        assert_eq!(t.id, deserialized.id);
        assert_eq!(t.number, deserialized.number);
        assert_eq!(t.integration_sha, deserialized.integration_sha);
        assert_eq!(t.bundle_ids, deserialized.bundle_ids);
        assert_eq!(t.validation_log, deserialized.validation_log);
        assert_eq!(t.status(), deserialized.status());
    }

    #[test]
    fn test_tick_unique_ids() {
        let t1 = Tick::new(1);
        let t2 = Tick::new(2);
        assert_ne!(t1.id, t2.id);
    }

    #[test]
    fn test_tick_number_preserved() {
        let t = Tick::new(42);
        assert_eq!(t.number, 42);
    }

    // FSM transition validation tests are in src/fsm/tests.rs (runtime interpreter).

    // --- Record trait tests ---

    #[test]
    fn test_record_id() {
        let t = Tick::new(1);
        assert_eq!(Record::id(&t), t.id);
    }

    #[test]
    fn test_record_updated_at() {
        let t = Tick::new(1);
        assert_eq!(Record::updated_at(&t), t.updated_at);
    }

    #[test]
    fn test_record_collection_name() {
        assert_eq!(Tick::collection_name(), "ticks");
    }

    #[test]
    fn test_record_indexed_fields() {
        let t = Tick::new(42);
        let fields = t.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Open".to_string())));
        assert_eq!(fields.get("number"), Some(&IndexValue::Int(42)));
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn test_record_indexed_fields_reflect_status() {
        let mut t = Tick::new(1);
        t.force_status(TickStatus::Published);
        let fields = t.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Published".to_string())));
    }
}
