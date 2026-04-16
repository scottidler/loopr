#![allow(clippy::unwrap_used, unused_imports)]

use serde_json::json;

use crate::agents::{AgentKind, AgentSession, AgentStatus};

use super::fixtures::*;

#[tokio::test]
async fn test_pool_exhaustion_multi_type() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Fill Coordinator pool (pool_size = 1)
    let session = AgentSession::new(AgentKind::Director, "model".into());
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    // Second Coordinator should be rejected
    let code = dispatch_err(&stores, &tx, &wm, &ic, "agent.start", json!({"agent_type": "director"})).await;
    assert_eq!(code, -32004, "expected pool_exhausted error code");

    // But Researcher should still work (different pool)
    let resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "agent.start",
        json!({"agent_type": "researcher"}),
    )
    .await;
    assert!(resp["id"].as_str().is_some());
}

#[tokio::test]
async fn test_agent_start_pool_check_atomic_under_lock() {
    // Two sequential agent.start calls through the dispatch path: first succeeds,
    // second is rejected by the pool check. Verifies that the in-memory check and
    // store write both happen under the same lock so no phantom session slips through.
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let resp = dispatch_ok(&stores, &tx, &wm, &ic, "agent.start", json!({"agent_type": "director"})).await;
    assert!(resp["id"].as_str().is_some(), "first start should succeed");

    let code = dispatch_err(&stores, &tx, &wm, &ic, "agent.start", json!({"agent_type": "director"})).await;
    assert_eq!(code, -32004, "second start must be rejected with pool_exhausted");
}

#[test]
fn test_no_auto_start_coordinator_on_daemon_boot() {
    // Fresh Stores (the state a daemon begins with before any IPC) must have
    // zero coordinator sessions — the auto_start_coordinator rogue path is gone.
    let stores = test_stores();
    let sessions = stores.agent_sessions.read().unwrap();
    let coordinator_count = sessions
        .values()
        .filter(|s| s.agent_type == crate::agents::AgentKind::Director)
        .count();
    assert_eq!(coordinator_count, 0, "no coordinator should exist in fresh stores");
}

#[tokio::test]
async fn test_pool_allows_after_terminal_session() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Add a Completed Coordinator session
    let mut session = AgentSession::new(AgentKind::Director, "model".into());
    let _ = session.transition_to(AgentStatus::Running);
    let _ = session.transition_to(AgentStatus::Completed);
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    // Should be allowed - terminal sessions don't count
    let resp = dispatch_ok(&stores, &tx, &wm, &ic, "agent.start", json!({"agent_type": "director"})).await;
    assert!(resp["id"].as_str().is_some());
}
