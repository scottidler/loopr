#![allow(clippy::unwrap_used)]

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
    let session = AgentSession::new(AgentKind::Coordinator, "model".into());
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    // Second Coordinator should be rejected
    let code = dispatch_err(
        &stores,
        &tx,
        &wm,
        &ic,
        "agent.start",
        json!({"agent_type": "coordinator"}),
    );
    assert_eq!(code, -32004, "expected pool_exhausted error code");

    // But Researcher should still work (different pool)
    let resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "agent.start",
        json!({"agent_type": "researcher"}),
    );
    assert!(resp["id"].as_str().is_some());
}

#[tokio::test]
async fn test_pool_allows_after_terminal_session() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Add a Completed Coordinator session
    let mut session = AgentSession::new(AgentKind::Coordinator, "model".into());
    let _ = session.transition_to(AgentStatus::Running);
    let _ = session.transition_to(AgentStatus::Completed);
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    // Should be allowed - terminal sessions don't count
    let resp = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "agent.start",
        json!({"agent_type": "coordinator"}),
    );
    assert!(resp["id"].as_str().is_some());
}
