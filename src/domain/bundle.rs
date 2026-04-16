use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use loopr_derive::FlexibleEnum;

use crate::id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, FlexibleEnum)]
pub enum BundleStatus {
    Proposed,
    Triaged,
    Reviewed,
    Accepted,
    Integrating,
    Merged,
    Rejected,
    Superseded,
}

impl std::fmt::Display for BundleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Backward-compatible deserialization for claims: accepts both String and Vec<String>.
fn deserialize_claims<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    struct ClaimsVisitor;
    impl<'de> de::Visitor<'de> for ClaimsVisitor {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a string or array of strings")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(if v.is_empty() { Vec::new() } else { vec![v.to_string()] })
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                v.push(s);
            }
            Ok(v)
        }
    }
    deserializer.deserialize_any(ClaimsVisitor)
}

/// A proposed change set produced from a worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub id: String,
    pub work_id: String,
    pub base_tick_id: Option<String>,
    pub branch_name: String,
    pub paths: Vec<String>,
    #[serde(deserialize_with = "deserialize_claims")]
    pub claims: Vec<String>,
    pub verification: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Total lines changed (insertions + deletions) from git diff --stat.
    #[serde(default)]
    pub loc_changed: Option<u32>,
    #[serde(default)]
    pub locks_used: Vec<String>,
    /// If set, this bundle is a no-op: the Implementer claims the work is already
    /// complete without code changes. The Reviewer must verify the codebase state.
    #[serde(default)]
    pub noop_reason: Option<String>,
    /// SHA of the worktree HEAD at bundle proposal time. Used for audit
    /// and pre-merge verification that the branch still has the expected commits.
    #[serde(default)]
    pub head_commit: Option<String>,
    /// `true` when this bundle's claims directly address a prior rejection for the same
    /// Work item using structural keywords (signature, interface contract, etc.).
    /// Disputed bundles are routed to an arbitrator instead of the normal reviewer queue.
    #[serde(default)]
    pub disputed: bool,
    status: BundleStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Bundle {
    /// Read current status.
    pub fn status(&self) -> BundleStatus {
        self.status
    }

    /// Validated FSM transition via the runtime interpreter.
    pub fn transition(
        &mut self,
        target: BundleStatus,
        role: crate::domain::role::Role,
        fsm: &crate::fsm::runtime::FsmInterpreter,
    ) -> eyre::Result<crate::domain::transition::Transition> {
        use crate::fsm::status::FsmStatus;
        let result = fsm.validate_transition(
            BundleStatus::fsm_name(),
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
    pub fn force_status(&mut self, target: BundleStatus) {
        self.status = target;
        self.updated_at = id::now_millis();
    }

    pub fn new(work_id: String, base_tick_id: Option<String>, branch_name: String, claims: Vec<String>) -> Self {
        tracing::debug!("Bundle::new(work_id={}, branch_name={})", work_id, branch_name);
        let now = id::now_millis();
        Self {
            id: id::generate_id("bd"),
            work_id,
            base_tick_id,
            branch_name,
            paths: Vec::new(),
            claims,
            verification: String::new(),
            description: None,
            loc_changed: None,
            locks_used: Vec::new(),
            noop_reason: None,
            head_commit: None,
            disputed: false,
            status: BundleStatus::Proposed,
            created_at: now,
            updated_at: now,
        }
    }
}

impl Record for Bundle {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "bundles"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m.insert("work_id".into(), IndexValue::String(self.work_id.clone()));
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundle_status_display() {
        assert_eq!(BundleStatus::Proposed.to_string(), "Proposed");
        assert_eq!(BundleStatus::Integrating.to_string(), "Integrating");
        assert_eq!(BundleStatus::Merged.to_string(), "Merged");
        assert_eq!(BundleStatus::Superseded.to_string(), "Superseded");
    }

    #[test]
    fn test_bundle_status_serde_roundtrip() {
        let status = BundleStatus::Accepted;
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: BundleStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, deserialized);
    }

    #[test]
    fn test_bundle_status_display_matches_serde() {
        // Regression: Display must produce values that serde can deserialize.
        for status in [
            BundleStatus::Proposed,
            BundleStatus::Triaged,
            BundleStatus::Reviewed,
            BundleStatus::Accepted,
            BundleStatus::Integrating,
            BundleStatus::Merged,
            BundleStatus::Rejected,
            BundleStatus::Superseded,
        ] {
            let display = status.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: BundleStatus = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("Display output '{}' not deserializable: {}", display, e));
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_bundle_new() {
        let b = Bundle::new(
            "wi-123".to_string(),
            Some("tick-001".to_string()),
            "feature/jwt".to_string(),
            vec!["Add JWT signing".into()],
        );
        assert_eq!(b.work_id, "wi-123");
        assert_eq!(b.base_tick_id, Some("tick-001".to_string()));
        assert_eq!(b.branch_name, "feature/jwt");
        assert_eq!(b.claims, vec!["Add JWT signing".to_string()]);
        assert!(b.verification.is_empty());
        assert_eq!(b.status(), BundleStatus::Proposed);
        assert!(b.paths.is_empty());
        assert!(!b.id.is_empty());
        assert!(b.created_at > 0);
        assert_eq!(b.created_at, b.updated_at);
    }

    #[test]
    fn test_bundle_new_no_base_tick() {
        let b = Bundle::new(
            "wi-456".to_string(),
            None,
            "feature/init".to_string(),
            vec!["Initial setup".into()],
        );
        assert!(b.base_tick_id.is_none());
    }

    #[test]
    fn test_bundle_serde_roundtrip() {
        let mut b = Bundle::new(
            "wi-789".to_string(),
            Some("tick-002".to_string()),
            "fix/auth".to_string(),
            vec!["Fix auth bug".into()],
        );
        b.paths = vec!["src/auth.rs".to_string(), "src/main.rs".to_string()];
        b.verification = "cargo test passed".to_string();

        let json = serde_json::to_string(&b).unwrap();
        let deserialized: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(b.id, deserialized.id);
        assert_eq!(b.work_id, deserialized.work_id);
        assert_eq!(b.base_tick_id, deserialized.base_tick_id);
        assert_eq!(b.branch_name, deserialized.branch_name);
        assert_eq!(b.paths, deserialized.paths);
        assert_eq!(b.claims, deserialized.claims);
        assert_eq!(b.verification, deserialized.verification);
        assert_eq!(b.status(), deserialized.status());
    }

    #[test]
    fn test_bundle_serde_roundtrip_with_head_commit() {
        let mut b = Bundle::new(
            "wi-hc".to_string(),
            None,
            "agent/wi-hc".to_string(),
            vec!["test claim".into()],
        );
        b.head_commit = Some("abc123def456".to_string());

        let json = serde_json::to_string(&b).unwrap();
        let deserialized: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.head_commit, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_bundle_serde_backward_compat_without_head_commit() {
        // Old JSON without head_commit should deserialize with None
        let json = serde_json::json!({
            "id": "bd-test",
            "work_id": "wi-1",
            "base_tick_id": null,
            "branch_name": "agent/wi-1",
            "paths": [],
            "claims": [],
            "verification": "",
            "status": "Proposed",
            "created_at": 1000,
            "updated_at": 1000
        });
        let bundle: Bundle = serde_json::from_value(json).unwrap();
        assert!(bundle.head_commit.is_none());
    }

    #[test]
    fn test_bundle_serde_backward_compat_ignores_loose_files() {
        // Old JSONL records with a "loose_files" field must still deserialize (serde ignores unknown fields).
        let json = serde_json::json!({
            "id": "bd-test",
            "work_id": "wi-1",
            "base_tick_id": null,
            "branch_name": "agent/wi-1",
            "paths": [],
            "claims": [],
            "verification": "",
            "loose_files": ["src/extra.rs"],
            "status": "Proposed",
            "created_at": 1000,
            "updated_at": 1000
        });
        let bundle: Bundle = serde_json::from_value(json).unwrap();
        assert_eq!(bundle.id, "bd-test");
    }

    #[test]
    fn test_bundle_unique_ids() {
        let b1 = Bundle::new("wi".to_string(), None, "a".to_string(), vec![]);
        let b2 = Bundle::new("wi".to_string(), None, "b".to_string(), vec![]);
        assert_ne!(b1.id, b2.id);
    }

    // FSM transition validation tests are in src/fsm/tests.rs (runtime interpreter).

    #[test]
    fn test_is_terminal() {
        use crate::fsm::status::FsmStatus;
        let fsm = crate::fsm::runtime::FsmInterpreter::embedded().unwrap();
        assert!(!BundleStatus::Proposed.is_terminal(&fsm));
        assert!(!BundleStatus::Triaged.is_terminal(&fsm));
        assert!(!BundleStatus::Integrating.is_terminal(&fsm));
        assert!(BundleStatus::Merged.is_terminal(&fsm));
        assert!(BundleStatus::Rejected.is_terminal(&fsm));
        assert!(BundleStatus::Superseded.is_terminal(&fsm));
    }

    // --- Record trait tests ---

    #[test]
    fn test_record_id() {
        let b = Bundle::new("wi-1".into(), None, "branch".into(), vec!["claims".into()]);
        assert_eq!(Record::id(&b), b.id);
    }

    #[test]
    fn test_record_updated_at() {
        let b = Bundle::new("wi-1".into(), None, "branch".into(), vec!["claims".into()]);
        assert_eq!(Record::updated_at(&b), b.updated_at);
    }

    #[test]
    fn test_record_collection_name() {
        assert_eq!(Bundle::collection_name(), "bundles");
    }

    #[test]
    fn test_record_indexed_fields() {
        let b = Bundle::new("wi-1".into(), None, "branch".into(), vec!["claims".into()]);
        let fields = b.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Proposed".to_string())));
        assert_eq!(fields.get("work_id"), Some(&IndexValue::String("wi-1".to_string())));
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn test_record_indexed_fields_reflect_status() {
        let mut b = Bundle::new("wi-1".into(), None, "branch".into(), vec!["claims".into()]);
        b.force_status(BundleStatus::Merged);
        let fields = b.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Merged".to_string())));
    }

    // --- M1: Claims backward compatibility tests ---

    #[test]
    fn test_claims_deserialize_from_string() {
        let json = r#"{"id":"b-1","work_id":"wi-1","base_tick_id":null,"branch_name":"b","paths":[],"claims":"old string claim","verification":"","status":"Proposed","created_at":1,"updated_at":1}"#;
        let b: Bundle = serde_json::from_str(json).unwrap();
        assert_eq!(b.claims, vec!["old string claim".to_string()]);
    }

    #[test]
    fn test_claims_deserialize_from_array() {
        let json = r#"{"id":"b-2","work_id":"wi-1","base_tick_id":null,"branch_name":"b","paths":[],"claims":["c1","c2"],"verification":"","status":"Proposed","created_at":1,"updated_at":1}"#;
        let b: Bundle = serde_json::from_str(json).unwrap();
        assert_eq!(b.claims, vec!["c1".to_string(), "c2".to_string()]);
    }

    #[test]
    fn test_claims_deserialize_from_empty_string() {
        let json = r#"{"id":"b-3","work_id":"wi-1","base_tick_id":null,"branch_name":"b","paths":[],"claims":"","verification":"","status":"Proposed","created_at":1,"updated_at":1}"#;
        let b: Bundle = serde_json::from_str(json).unwrap();
        assert!(b.claims.is_empty());
    }

    #[test]
    fn test_claims_vec_roundtrip() {
        let b = Bundle::new("wi-1".into(), None, "b".into(), vec!["claim1".into(), "claim2".into()]);
        let json = serde_json::to_string(&b).unwrap();
        let restored: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.claims, vec!["claim1".to_string(), "claim2".to_string()]);
    }

    #[test]
    fn test_noop_reason_serde_roundtrip() {
        let mut b = Bundle::new("wi-1".into(), None, String::new(), vec!["already done".into()]);
        b.noop_reason = Some("Phase 1 over-delivered".to_string());
        let json = serde_json::to_string(&b).unwrap();
        let restored: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.noop_reason.as_deref(), Some("Phase 1 over-delivered"));
        assert!(restored.branch_name.is_empty());
    }

    #[test]
    fn test_noop_reason_absent_deserializes_as_none() {
        // Backward compat: old bundles without noop_reason should deserialize fine
        let json = r#"{
            "id": "bd-test",
            "work_id": "wi-1",
            "base_tick_id": null,
            "branch_name": "agent/wi-1",
            "paths": [],
            "claims": ["claim"],
            "verification": "",
            "status": "Proposed",
            "created_at": 0,
            "updated_at": 0
        }"#;
        let b: Bundle = serde_json::from_str(json).unwrap();
        assert!(b.noop_reason.is_none());
    }
}
