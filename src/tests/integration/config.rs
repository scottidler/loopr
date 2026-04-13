#![allow(clippy::unwrap_used)]

use serde_json::json;

use crate::config::Config;
use crate::daemon::handlers::dispatch;
use crate::fsm::runtime::FsmInterpreter;
use crate::ipc::protocol::DaemonRequest;

use super::fixtures::*;

#[test]
fn test_strategy_knobs_defaults() {
    use crate::config::{ConflictPolicy, StalePolicy, StrategyConfig, TickCadence, ValidatorStrictness};

    let config = StrategyConfig::default();

    assert!(matches!(config.stale_policy, StalePolicy::ReplanAtSafePoint));
    assert!(matches!(config.conflict_policy, ConflictPolicy::LockAdvisory));
    assert!(matches!(config.tick_cadence, TickCadence::Continuous));
    assert_eq!(config.bundle_size.max_files_touched, 8);
    assert_eq!(config.bundle_size.max_loc_changed, 300);
    assert!(matches!(
        config.validator_strictness,
        ValidatorStrictness::HardFailOnAnyAmbiguity
    ));
    assert!(config.promotion.auto_promote);
    assert_eq!(config.promotion.min_reinforcements, 3);
    assert_eq!(config.max_lock_ttl_minutes, 60);
}

#[test]
fn test_agents_disabled_by_default() {
    let config = Config::default();

    assert!(!config.agents.enabled, "agents should be disabled by default");
    assert!(!config.integrator.enabled, "integrator should be disabled by default");
}

#[tokio::test]
async fn test_dispatch_routes_mvp4_methods() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // All these methods should NOT return "method not found"
    let methods = vec![
        "lock.create",
        "lock.list",
        "lock.release",
        "lock.expire",
        "agent.start",
        "agent.stop",
        "agent.pause",
        "agent.resume",
        "agent.status",
        "agent.list",
    ];

    let fsm = FsmInterpreter::embedded().unwrap();
    for method in methods {
        let req = DaemonRequest::new(1, method, json!({}));
        let resp = dispatch(&stores, &tx, &wm, &ic, &fsm, req).await;
        // May fail with invalid_params, but should NOT fail with method_not_found
        if let Some(err) = &resp.error {
            assert_ne!(err.code, -32601, "{method} returned method_not_found");
        }
    }
}

#[test]
fn test_max_requeries_config_defaults() {
    use crate::config::AgentRoleConfig;

    // All roles default to max_requeries=3
    assert_eq!(AgentRoleConfig::default_implementer().max_requeries, 3);
    assert_eq!(AgentRoleConfig::default_reviewer().max_requeries, 3);
    assert_eq!(AgentRoleConfig::default_researcher().max_requeries, 3);
}
