//! Phase 2 smoke tests: compile the DI surface + default config.
//! Deeper coverage lands with the `integrate` body in Phase 3.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use crate::{IntegrationError, IntegratorConfig};

#[test]
fn integrator_config_defaults() {
    let cfg = IntegratorConfig::default();
    assert_eq!(cfg.git_timeout, Duration::from_secs(60));
    assert!(!cfg.allow_multi_bundle);
}

#[test]
fn integration_error_display_covers_every_variant() {
    // Typed-variants-exist smoke: instantiate each variant and confirm
    // Display produces something non-empty. The point is to fail the
    // test suite if a variant is renamed or the #[error(...)] attribute
    // drops a field, so downstream daemon-wiring matches don't drift.
    use domain::BundleStatus;

    let cases: Vec<(String, &str)> = vec![
        (IntegrationError::NoBundles.to_string(), "no bundles"),
        (IntegrationError::MultiBundleNotSupported { count: 3 }.to_string(), "3"),
        (
            IntegrationError::BundleNotAccepted {
                bundle_id: "bd-abc".to_string(),
                current: BundleStatus::Proposed,
            }
            .to_string(),
            "bd-abc",
        ),
        (
            IntegrationError::WorkNotFound {
                bundle_id: "bd-abc".to_string(),
                work_id: "wk-xyz".to_string(),
            }
            .to_string(),
            "wk-xyz",
        ),
        (
            IntegrationError::PlanBundleMismatch {
                bundle_id: "bd-abc".to_string(),
                work_plan_id: "pl-1".to_string(),
                plan_id: "pl-2".to_string(),
            }
            .to_string(),
            "pl-1",
        ),
        (
            IntegrationError::IntegrationBranchMissing {
                branch: "loopr/plan-xxx".to_string(),
            }
            .to_string(),
            "loopr/plan-xxx",
        ),
        (
            IntegrationError::EmptyBranch {
                bundle_id: "bd-abc".to_string(),
                branch: "loopr/wk-xyz".to_string(),
            }
            .to_string(),
            "loopr/wk-xyz",
        ),
        (
            IntegrationError::ConflictStructural {
                bundle_id: "bd-abc".to_string(),
                files: vec!["README.md".to_string()],
                peer_bundle_ids: vec!["bd-peer".to_string()],
            }
            .to_string(),
            "README.md",
        ),
        (
            IntegrationError::ConflictRetryable {
                bundle_id: "bd-abc".to_string(),
                branch: "loopr/wk-xyz".to_string(),
                stderr: "merge conflict".to_string(),
            }
            .to_string(),
            "merge conflict",
        ),
        (
            IntegrationError::Git("rev-parse failed".to_string()).to_string(),
            "rev-parse",
        ),
        (
            IntegrationError::Transition("Accepted -> Merged not allowed".to_string()).to_string(),
            "Accepted",
        ),
    ];
    for (msg, needle) in cases {
        assert!(
            msg.contains(needle),
            "Display for IntegrationError variant missing needle '{needle}': {msg}"
        );
    }
}

// Signature smoke: the `integrate` entry point compiles with real
// `store::Store` as the source of all three traits.
#[allow(dead_code)]
fn compiles_with_real_store(store: &store::Store) {
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let _deps = crate::IntegratorDeps {
        bundle_sink: store,
        works: store,
        ticks: store,
        config: IntegratorConfig::default(),
        target: PathBuf::from("/tmp"),
        git_lock: Arc::new(Mutex::new(())),
    };
}
