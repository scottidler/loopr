#![allow(clippy::unwrap_used, unused_imports)]

use serde_json::json;

use crate::agents::AgentStatus;
use crate::domain::tick::TickStatus;

use super::fixtures::*;

#[tokio::test]
async fn test_multi_agent_session_coexistence() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Start sessions of different types (use types that don't require work_id/bundle_id)
    let coord = dispatch_ok(&stores, &tx, &wm, &ic, "agent.start", json!({"agent_type": "director"})).await;
    let researcher1 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "agent.start",
        json!({"agent_type": "researcher"}),
    )
    .await;
    let researcher2 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "agent.start",
        json!({"agent_type": "researcher"}),
    )
    .await;

    // All should have unique session IDs
    let ids: Vec<&str> = [&coord, &researcher1, &researcher2]
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    let unique: std::collections::HashSet<&&str> = ids.iter().collect();
    assert_eq!(unique.len(), 3, "all session IDs should be unique");

    // List all agents
    let list = dispatch_ok(&stores, &tx, &wm, &ic, "agent.list", json!({})).await;
    assert_eq!(list.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn test_agent_pause_resume_lifecycle() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Start a coordinator
    let resp = dispatch_ok(&stores, &tx, &wm, &ic, "agent.start", json!({"agent_type": "director"})).await;
    let session_id = resp["id"].as_str().unwrap().to_string();

    // Transition session to Running first (agent.start creates in Starting state)
    {
        let mut sessions = stores.agent_sessions.write().unwrap();
        let session = sessions.get_mut(&session_id).unwrap();
        let _ = session.transition_to(AgentStatus::Running);
    }

    // Pause
    let paused = dispatch_ok(&stores, &tx, &wm, &ic, "agent.pause", json!({"session_id": session_id})).await;
    assert_eq!(paused["status"], "paused");

    // Resume
    let resumed = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "agent.resume",
        json!({"session_id": session_id}),
    )
    .await;
    assert_eq!(resumed["status"], "running");

    // Stop
    let stopped = dispatch_ok(&stores, &tx, &wm, &ic, "agent.stop", json!({"session_id": session_id})).await;
    assert_eq!(stopped["status"], "cancelled");
}

#[tokio::test]
async fn test_tick_lifecycle_via_dispatch() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create Tick
    let tick = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 1})).await;
    let tick_id = tick["id"].as_str().unwrap().to_string();
    assert_eq!(tick["status"], "Open");

    // Open -> Sealing
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick_id, "target_status": "Sealing", "role": "integrator"}),
    )
    .await;

    // Sealing -> Validating
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick_id, "target_status": "Validating", "role": "integrator"}),
    )
    .await;

    // Validating -> Published
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick_id, "target_status": "Published", "role": "integrator"}),
    )
    .await;

    // Verify final state
    let ticks = stores.ticks.read().unwrap();
    assert_eq!(ticks[&tick_id].status(), TickStatus::Published);
}
