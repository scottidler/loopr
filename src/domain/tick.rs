use serde::{Deserialize, Serialize};

use crate::domain::role::Role;
use crate::domain::transition::TransitionRule;
use crate::id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TickStatus {
    Open,
    Sealing,
    Validating,
    Published,
    Failed,
}

impl std::fmt::Display for TickStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Returns the FSM transition rules for Tick status.
/// All transitions are Integrator-only.
pub fn tick_transitions() -> Vec<TransitionRule<TickStatus>> {
    use TickStatus::*;
    vec![
        TransitionRule {
            from: Open,
            to: Sealing,
            role: Some(Role::Integrator),
        },
        TransitionRule {
            from: Sealing,
            to: Validating,
            role: Some(Role::Integrator),
        },
        TransitionRule {
            from: Validating,
            to: Published,
            role: Some(Role::Integrator),
        },
        TransitionRule {
            from: Validating,
            to: Failed,
            role: Some(Role::Integrator),
        },
    ]
}

/// An immutable integration checkpoint identified by a Git SHA.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tick {
    pub id: String,
    pub number: u32,
    pub integration_sha: Option<String>,
    pub bundle_ids: Vec<String>,
    pub validation_log: String,
    pub status: TickStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Tick {
    pub fn new(number: u32) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            number,
            integration_sha: None,
            bundle_ids: Vec::new(),
            validation_log: String::new(),
            status: TickStatus::Open,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transition::validate_transition;

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
    fn test_tick_new() {
        let t = Tick::new(1);
        assert_eq!(t.number, 1);
        assert!(t.integration_sha.is_none());
        assert!(t.bundle_ids.is_empty());
        assert!(t.validation_log.is_empty());
        assert_eq!(t.status, TickStatus::Open);
        assert!(!t.id.is_empty());
        assert!(t.created_at > 0);
        assert_eq!(t.created_at, t.updated_at);
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
        assert_eq!(t.status, deserialized.status);
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

    // --- Valid transitions ---

    #[test]
    fn test_valid_open_to_sealing() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Open, TickStatus::Sealing, Role::Integrator, &rules).is_ok());
    }

    #[test]
    fn test_valid_sealing_to_validating() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Sealing, TickStatus::Validating, Role::Integrator, &rules).is_ok());
    }

    #[test]
    fn test_valid_validating_to_published() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Validating, TickStatus::Published, Role::Integrator, &rules).is_ok());
    }

    #[test]
    fn test_valid_validating_to_failed() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Validating, TickStatus::Failed, Role::Integrator, &rules).is_ok());
    }

    // --- Invalid transitions: wrong role ---

    #[test]
    fn test_invalid_open_to_sealing_wrong_role() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Open, TickStatus::Sealing, Role::Coordinator, &rules).is_err());
        assert!(validate_transition(TickStatus::Open, TickStatus::Sealing, Role::Implementer, &rules).is_err());
    }

    #[test]
    fn test_invalid_sealing_to_validating_wrong_role() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Sealing, TickStatus::Validating, Role::Coordinator, &rules).is_err());
    }

    #[test]
    fn test_invalid_validating_to_published_wrong_role() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Validating, TickStatus::Published, Role::Implementer, &rules).is_err());
    }

    // --- Invalid transitions: skip states ---

    #[test]
    fn test_invalid_open_to_validating() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Open, TickStatus::Validating, Role::Integrator, &rules).is_err());
    }

    #[test]
    fn test_invalid_open_to_published() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Open, TickStatus::Published, Role::Integrator, &rules).is_err());
    }

    #[test]
    fn test_invalid_sealing_to_published() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Sealing, TickStatus::Published, Role::Integrator, &rules).is_err());
    }

    // --- Invalid transitions: terminal states ---

    #[test]
    fn test_invalid_published_to_anything() {
        let rules = tick_transitions();
        for target in [
            TickStatus::Open,
            TickStatus::Sealing,
            TickStatus::Validating,
            TickStatus::Failed,
        ] {
            assert!(
                validate_transition(TickStatus::Published, target, Role::Integrator, &rules).is_err(),
                "Expected Published→{:?} to fail",
                target
            );
        }
    }

    #[test]
    fn test_invalid_failed_to_anything() {
        let rules = tick_transitions();
        for target in [
            TickStatus::Open,
            TickStatus::Sealing,
            TickStatus::Validating,
            TickStatus::Published,
        ] {
            assert!(
                validate_transition(TickStatus::Failed, target, Role::Integrator, &rules).is_err(),
                "Expected Failed→{:?} to fail",
                target
            );
        }
    }

    // --- Invalid transitions: reverse direction ---

    #[test]
    fn test_invalid_sealing_to_open() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Sealing, TickStatus::Open, Role::Integrator, &rules).is_err());
    }

    #[test]
    fn test_invalid_validating_to_sealing() {
        let rules = tick_transitions();
        assert!(validate_transition(TickStatus::Validating, TickStatus::Sealing, Role::Integrator, &rules).is_err());
    }
}
