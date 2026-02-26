use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use crate::domain::role::Role;
use crate::domain::transition::TransitionRule;
use crate::id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Returns the FSM transition rules for Bundle status.
pub fn bundle_transitions() -> Vec<TransitionRule<BundleStatus>> {
    use BundleStatus::*;
    vec![
        // Happy path: Proposed → Triaged → Reviewed → Accepted → Integrating → Merged
        TransitionRule {
            from: Proposed,
            to: Triaged,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Triaged,
            to: Reviewed,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Triaged,
            to: Reviewed,
            role: Some(Role::Reviewer),
        },
        TransitionRule {
            from: Reviewed,
            to: Accepted,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Accepted,
            to: Integrating,
            role: Some(Role::Integrator),
        },
        TransitionRule {
            from: Integrating,
            to: Merged,
            role: Some(Role::Integrator),
        },
        // Rejection from Integrating
        TransitionRule {
            from: Integrating,
            to: Rejected,
            role: Some(Role::Integrator),
        },
        // Early rejection (Coordinator can reject from Proposed, Triaged, Reviewed)
        TransitionRule {
            from: Proposed,
            to: Rejected,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Triaged,
            to: Rejected,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Triaged,
            to: Rejected,
            role: Some(Role::Reviewer),
        },
        TransitionRule {
            from: Reviewed,
            to: Rejected,
            role: Some(Role::Coordinator),
        },
        // Superseded from any non-final state
        TransitionRule {
            from: Proposed,
            to: Superseded,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Triaged,
            to: Superseded,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Reviewed,
            to: Superseded,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Accepted,
            to: Superseded,
            role: Some(Role::Coordinator),
        },
        TransitionRule {
            from: Integrating,
            to: Superseded,
            role: Some(Role::Coordinator),
        },
    ]
}

/// A proposed change set produced from a worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub id: String,
    pub work_item_id: String,
    pub base_tick_id: Option<String>,
    pub branch_name: String,
    pub touched_paths: Vec<String>,
    pub claims: String,
    pub verification: String,
    pub status: BundleStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Bundle {
    pub fn new(work_item_id: String, base_tick_id: Option<String>, branch_name: String, claims: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            work_item_id,
            base_tick_id,
            branch_name,
            touched_paths: Vec::new(),
            claims,
            verification: String::new(),
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
        m.insert("work_item_id".into(), IndexValue::String(self.work_item_id.clone()));
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transition::validate_transition;

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
            "Add JWT signing".to_string(),
        );
        assert_eq!(b.work_item_id, "wi-123");
        assert_eq!(b.base_tick_id, Some("tick-001".to_string()));
        assert_eq!(b.branch_name, "feature/jwt");
        assert_eq!(b.claims, "Add JWT signing");
        assert!(b.verification.is_empty());
        assert_eq!(b.status, BundleStatus::Proposed);
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
            "Initial setup".to_string(),
        );
        assert!(b.base_tick_id.is_none());
    }

    #[test]
    fn test_bundle_serde_roundtrip() {
        let mut b = Bundle::new(
            "wi-789".to_string(),
            Some("tick-002".to_string()),
            "fix/auth".to_string(),
            "Fix auth bug".to_string(),
        );
        b.touched_paths = vec!["src/auth.rs".to_string(), "src/main.rs".to_string()];
        b.verification = "cargo test passed".to_string();

        let json = serde_json::to_string(&b).unwrap();
        let deserialized: Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(b.id, deserialized.id);
        assert_eq!(b.work_item_id, deserialized.work_item_id);
        assert_eq!(b.base_tick_id, deserialized.base_tick_id);
        assert_eq!(b.branch_name, deserialized.branch_name);
        assert_eq!(b.touched_paths, deserialized.touched_paths);
        assert_eq!(b.claims, deserialized.claims);
        assert_eq!(b.verification, deserialized.verification);
        assert_eq!(b.status, deserialized.status);
    }

    #[test]
    fn test_bundle_unique_ids() {
        let b1 = Bundle::new("wi".to_string(), None, "a".to_string(), "".to_string());
        let b2 = Bundle::new("wi".to_string(), None, "b".to_string(), "".to_string());
        assert_ne!(b1.id, b2.id);
    }

    // --- Valid transitions: happy path ---

    #[test]
    fn test_valid_proposed_to_triaged() {
        let rules = bundle_transitions();
        assert!(validate_transition(BundleStatus::Proposed, BundleStatus::Triaged, Role::Coordinator, &rules,).is_ok());
    }

    #[test]
    fn test_valid_triaged_to_reviewed() {
        let rules = bundle_transitions();
        assert!(validate_transition(BundleStatus::Triaged, BundleStatus::Reviewed, Role::Coordinator, &rules,).is_ok());
    }

    #[test]
    fn test_valid_reviewed_to_accepted() {
        let rules = bundle_transitions();
        assert!(
            validate_transition(
                BundleStatus::Reviewed,
                BundleStatus::Accepted,
                Role::Coordinator,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_valid_accepted_to_integrating() {
        let rules = bundle_transitions();
        assert!(
            validate_transition(
                BundleStatus::Accepted,
                BundleStatus::Integrating,
                Role::Integrator,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_valid_integrating_to_merged() {
        let rules = bundle_transitions();
        assert!(
            validate_transition(
                BundleStatus::Integrating,
                BundleStatus::Merged,
                Role::Integrator,
                &rules,
            )
            .is_ok()
        );
    }

    // --- Valid transitions: rejection ---

    #[test]
    fn test_valid_integrating_to_rejected() {
        let rules = bundle_transitions();
        assert!(
            validate_transition(
                BundleStatus::Integrating,
                BundleStatus::Rejected,
                Role::Integrator,
                &rules,
            )
            .is_ok()
        );
    }

    #[test]
    fn test_valid_early_rejection() {
        let rules = bundle_transitions();
        for from in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Reviewed] {
            assert!(
                validate_transition(from, BundleStatus::Rejected, Role::Coordinator, &rules,).is_ok(),
                "Expected {:?}→Rejected to succeed",
                from
            );
        }
    }

    // --- Valid transitions: superseded ---

    #[test]
    fn test_valid_superseded_from_non_final() {
        let rules = bundle_transitions();
        let non_final = [
            BundleStatus::Proposed,
            BundleStatus::Triaged,
            BundleStatus::Reviewed,
            BundleStatus::Accepted,
            BundleStatus::Integrating,
        ];
        for from in non_final {
            assert!(
                validate_transition(from, BundleStatus::Superseded, Role::Coordinator, &rules,).is_ok(),
                "Expected {:?}→Superseded to succeed",
                from
            );
        }
    }

    // --- Invalid transitions ---

    #[test]
    fn test_invalid_proposed_to_triaged_wrong_role() {
        let rules = bundle_transitions();
        assert!(
            validate_transition(BundleStatus::Proposed, BundleStatus::Triaged, Role::Implementer, &rules,).is_err()
        );
    }

    #[test]
    fn test_invalid_skip_proposed_to_accepted() {
        let rules = bundle_transitions();
        assert!(
            validate_transition(
                BundleStatus::Proposed,
                BundleStatus::Accepted,
                Role::Coordinator,
                &rules,
            )
            .is_err()
        );
    }

    #[test]
    fn test_invalid_merged_to_anything() {
        let rules = bundle_transitions();
        for target in [BundleStatus::Proposed, BundleStatus::Triaged, BundleStatus::Integrating] {
            assert!(
                validate_transition(BundleStatus::Merged, target, Role::Coordinator, &rules,).is_err(),
                "Expected Merged→{:?} to fail",
                target
            );
        }
    }

    #[test]
    fn test_invalid_rejected_to_anything() {
        let rules = bundle_transitions();
        assert!(
            validate_transition(
                BundleStatus::Rejected,
                BundleStatus::Proposed,
                Role::Coordinator,
                &rules,
            )
            .is_err()
        );
    }

    #[test]
    fn test_invalid_superseded_to_anything() {
        let rules = bundle_transitions();
        assert!(
            validate_transition(
                BundleStatus::Superseded,
                BundleStatus::Proposed,
                Role::Coordinator,
                &rules,
            )
            .is_err()
        );
    }

    #[test]
    fn test_invalid_accepted_to_integrating_wrong_role() {
        let rules = bundle_transitions();
        // Only Integrator can move Accepted → Integrating
        assert!(
            validate_transition(
                BundleStatus::Accepted,
                BundleStatus::Integrating,
                Role::Coordinator,
                &rules,
            )
            .is_err()
        );
    }

    #[test]
    fn test_invalid_integrating_to_merged_wrong_role() {
        let rules = bundle_transitions();
        // Only Integrator can move Integrating → Merged
        assert!(
            validate_transition(
                BundleStatus::Integrating,
                BundleStatus::Merged,
                Role::Coordinator,
                &rules,
            )
            .is_err()
        );
    }

    // --- Record trait tests ---

    #[test]
    fn test_record_id() {
        let b = Bundle::new("wi-1".into(), None, "branch".into(), "claims".into());
        assert_eq!(Record::id(&b), b.id);
    }

    #[test]
    fn test_record_updated_at() {
        let b = Bundle::new("wi-1".into(), None, "branch".into(), "claims".into());
        assert_eq!(Record::updated_at(&b), b.updated_at);
    }

    #[test]
    fn test_record_collection_name() {
        assert_eq!(Bundle::collection_name(), "bundles");
    }

    #[test]
    fn test_record_indexed_fields() {
        let b = Bundle::new("wi-1".into(), None, "branch".into(), "claims".into());
        let fields = b.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Proposed".to_string())));
        assert_eq!(
            fields.get("work_item_id"),
            Some(&IndexValue::String("wi-1".to_string()))
        );
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn test_record_indexed_fields_reflect_status() {
        let mut b = Bundle::new("wi-1".into(), None, "branch".into(), "claims".into());
        b.status = BundleStatus::Merged;
        let fields = b.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("Merged".to_string())));
    }
}
