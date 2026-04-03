use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use loopr_derive::{FlexibleEnum, Fsm};

use crate::id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, FlexibleEnum, Fsm)]
pub enum BundleStatus {
    #[transitions(Triaged(Coordinator), Rejected(Coordinator, Reviewer), Superseded(Coordinator))]
    Proposed,
    #[transitions(
        Reviewed(Coordinator, Reviewer),
        Accepted(Coordinator),
        Rejected(Coordinator, Reviewer),
        Superseded(Coordinator)
    )]
    Triaged,
    #[transitions(Accepted(Coordinator), Rejected(Coordinator, Reviewer), Superseded(Coordinator))]
    Reviewed,
    #[transitions(Integrating(Integrator), Rejected(Integrator), Superseded(Coordinator))]
    Accepted,
    #[transitions(Merged(Integrator), Rejected(Integrator), Superseded(Coordinator))]
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
    pub touched_paths: Vec<String>,
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
    /// Files modified in the worktree but excluded from the bundle because
    /// they fall outside the Work's resource_tags scope. Observable signal
    /// for downstream agents to detect scope gaps.
    #[serde(default)]
    pub loose_files: Vec<String>,
    status: BundleStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Bundle {
    /// Read current status.
    pub fn status(&self) -> BundleStatus {
        self.status
    }

    /// Validated FSM transition. Returns Err if invalid.
    pub fn transition(
        &mut self,
        target: BundleStatus,
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
    pub fn force_status(&mut self, target: BundleStatus) {
        self.status = target;
        self.updated_at = id::now_millis();
    }

    pub fn new(work_id: String, base_tick_id: Option<String>, branch_name: String, claims: Vec<String>) -> Self {
        log::debug!("Bundle::new(work_id={}, branch_name={})", work_id, branch_name);
        let now = id::now_millis();
        Self {
            id: id::generate_id("bd"),
            work_id,
            base_tick_id,
            branch_name,
            touched_paths: Vec::new(),
            claims,
            verification: String::new(),
            description: None,
            loc_changed: None,
            locks_used: Vec::new(),
            noop_reason: None,
            head_commit: None,
            loose_files: Vec::new(),
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
    use crate::domain::role::Role;
    use crate::domain::transition::Transition;

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
        assert!(b.touched_paths.is_empty());
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
        b.touched_paths = vec!["src/auth.rs".to_string(), "src/main.rs".to_string()];
        b.verification = "cargo test passed".to_string();

        let json = serde_json::to_string(&b).unwrap();
        let deserialized: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(b.id, deserialized.id);
        assert_eq!(b.work_id, deserialized.work_id);
        assert_eq!(b.base_tick_id, deserialized.base_tick_id);
        assert_eq!(b.branch_name, deserialized.branch_name);
        assert_eq!(b.touched_paths, deserialized.touched_paths);
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
            "touched_paths": [],
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
    fn test_bundle_unique_ids() {
        let b1 = Bundle::new("wi".to_string(), None, "a".to_string(), vec![]);
        let b2 = Bundle::new("wi".to_string(), None, "b".to_string(), vec![]);
        assert_ne!(b1.id, b2.id);
    }

    // --- Valid transitions: happy path ---

    #[test]
    fn test_valid_proposed_to_triaged() {
        assert!(
            BundleStatus::Proposed
                .validate_transition(BundleStatus::Triaged, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_triaged_to_reviewed() {
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Reviewed, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_reviewed_to_accepted() {
        assert!(
            BundleStatus::Reviewed
                .validate_transition(BundleStatus::Accepted, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_accepted_to_integrating() {
        assert!(
            BundleStatus::Accepted
                .validate_transition(BundleStatus::Integrating, Role::Integrator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_integrating_to_merged() {
        assert!(
            BundleStatus::Integrating
                .validate_transition(BundleStatus::Merged, Role::Integrator)
                .is_ok()
        );
    }

    // --- Valid transitions: advisory review (Triaged -> Accepted) ---

    #[test]
    fn test_valid_triaged_to_accepted_coordinator() {
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Accepted, Role::Coordinator)
                .is_ok()
        );
    }

    #[test]
    fn test_invalid_triaged_to_accepted_wrong_role() {
        // Only Coordinator can bypass review
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Accepted, Role::Implementer)
                .is_err()
        );
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Accepted, Role::Reviewer)
                .is_err()
        );
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Accepted, Role::Researcher)
                .is_err()
        );
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Accepted, Role::Integrator)
                .is_err()
        );
    }

    #[test]
    fn test_advisory_bypass_does_not_break_normal_happy_path() {
        assert!(
            BundleStatus::Proposed
                .validate_transition(BundleStatus::Triaged, Role::Coordinator)
                .is_ok()
        );
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Reviewed, Role::Reviewer)
                .is_ok()
        );
        assert!(
            BundleStatus::Reviewed
                .validate_transition(BundleStatus::Accepted, Role::Coordinator)
                .is_ok()
        );
        assert!(
            BundleStatus::Accepted
                .validate_transition(BundleStatus::Integrating, Role::Integrator)
                .is_ok()
        );
        assert!(
            BundleStatus::Integrating
                .validate_transition(BundleStatus::Merged, Role::Integrator)
                .is_ok()
        );
    }

    #[test]
    fn test_advisory_bypass_path_continues_to_integrating() {
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Accepted, Role::Coordinator)
                .is_ok()
        );
        assert!(
            BundleStatus::Accepted
                .validate_transition(BundleStatus::Integrating, Role::Integrator)
                .is_ok()
        );
        assert!(
            BundleStatus::Integrating
                .validate_transition(BundleStatus::Merged, Role::Integrator)
                .is_ok()
        );
    }

    #[test]
    fn test_advisory_accepted_can_still_be_rejected() {
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Accepted, Role::Coordinator)
                .is_ok()
        );
        assert!(
            BundleStatus::Accepted
                .validate_transition(BundleStatus::Rejected, Role::Integrator)
                .is_ok()
        );
    }

    #[test]
    fn test_advisory_accepted_can_be_superseded() {
        assert!(
            BundleStatus::Triaged
                .validate_transition(BundleStatus::Accepted, Role::Coordinator)
                .is_ok()
        );
        assert!(
            BundleStatus::Accepted
                .validate_transition(BundleStatus::Superseded, Role::Coordinator)
                .is_ok()
        );
    }

    // --- Valid transitions: rejection ---

    #[test]
    fn test_valid_integrating_to_rejected() {
        assert!(
            BundleStatus::Integrating
                .validate_transition(BundleStatus::Rejected, Role::Integrator)
                .is_ok()
        );
    }

    #[test]
    fn test_valid_early_rejection() {
        for from in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Reviewed] {
            assert!(
                from.validate_transition(BundleStatus::Rejected, Role::Coordinator)
                    .is_ok(),
                "Expected {:?}->Rejected to succeed",
                from
            );
        }
    }

    // --- Valid transitions: superseded ---

    #[test]
    fn test_valid_superseded_from_non_final() {
        let non_final = [
            BundleStatus::Proposed,
            BundleStatus::Triaged,
            BundleStatus::Reviewed,
            BundleStatus::Accepted,
            BundleStatus::Integrating,
        ];
        for from in non_final {
            assert!(
                from.validate_transition(BundleStatus::Superseded, Role::Coordinator)
                    .is_ok(),
                "Expected {:?}->Superseded to succeed",
                from
            );
        }
    }

    // --- Invalid transitions ---

    #[test]
    fn test_invalid_proposed_to_triaged_wrong_role() {
        assert!(
            BundleStatus::Proposed
                .validate_transition(BundleStatus::Triaged, Role::Implementer)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_skip_proposed_to_accepted() {
        assert!(
            BundleStatus::Proposed
                .validate_transition(BundleStatus::Accepted, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_merged_to_anything() {
        for target in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Integrating] {
            assert!(
                BundleStatus::Merged
                    .validate_transition(target, Role::Coordinator)
                    .is_err(),
                "Expected Merged->{:?} to fail",
                target
            );
        }
    }

    #[test]
    fn test_invalid_rejected_to_anything() {
        assert!(
            BundleStatus::Rejected
                .validate_transition(BundleStatus::Proposed, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_superseded_to_anything() {
        assert!(
            BundleStatus::Superseded
                .validate_transition(BundleStatus::Proposed, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_accepted_to_integrating_wrong_role() {
        assert!(
            BundleStatus::Accepted
                .validate_transition(BundleStatus::Integrating, Role::Coordinator)
                .is_err()
        );
    }

    #[test]
    fn test_invalid_integrating_to_merged_wrong_role() {
        assert!(
            BundleStatus::Integrating
                .validate_transition(BundleStatus::Merged, Role::Coordinator)
                .is_err()
        );
    }

    // --- Terminal state and idempotency tests ---

    #[test]
    fn test_is_terminal() {
        assert!(!BundleStatus::Proposed.is_terminal());
        assert!(!BundleStatus::Triaged.is_terminal());
        assert!(!BundleStatus::Integrating.is_terminal());
        assert!(BundleStatus::Merged.is_terminal());
        assert!(BundleStatus::Rejected.is_terminal());
        assert!(BundleStatus::Superseded.is_terminal());
    }

    #[test]
    fn test_idempotent_self_transition() {
        let r = BundleStatus::Proposed.validate_transition(BundleStatus::Proposed, Role::Coordinator);
        assert_eq!(r.unwrap(), Transition::Unchanged);
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
        let json = r#"{"id":"b-1","work_id":"wi-1","base_tick_id":null,"branch_name":"b","touched_paths":[],"claims":"old string claim","verification":"","status":"Proposed","created_at":1,"updated_at":1}"#;
        let b: Bundle = serde_json::from_str(json).unwrap();
        assert_eq!(b.claims, vec!["old string claim".to_string()]);
    }

    #[test]
    fn test_claims_deserialize_from_array() {
        let json = r#"{"id":"b-2","work_id":"wi-1","base_tick_id":null,"branch_name":"b","touched_paths":[],"claims":["c1","c2"],"verification":"","status":"Proposed","created_at":1,"updated_at":1}"#;
        let b: Bundle = serde_json::from_str(json).unwrap();
        assert_eq!(b.claims, vec!["c1".to_string(), "c2".to_string()]);
    }

    #[test]
    fn test_claims_deserialize_from_empty_string() {
        let json = r#"{"id":"b-3","work_id":"wi-1","base_tick_id":null,"branch_name":"b","touched_paths":[],"claims":"","verification":"","status":"Proposed","created_at":1,"updated_at":1}"#;
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
            "touched_paths": [],
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
