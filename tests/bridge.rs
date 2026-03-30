//! Integration tests: coordinator.accept_plan bridge
//!
//! Deterministic tests that verify the `coordinator.accept_plan` handler
//! correctly creates Plans, CoordinatorGoals, and starts the Coordinator
//! agent. No LLM calls - all tests run fast (<2s) without ANTHROPIC_API_KEY.

#![allow(clippy::unwrap_used)]

mod common;

use std::time::Duration;

use eyre::{Result, eyre};
use loopr::ipc::client::IpcClient;
use loopr::ipc::protocol::{DaemonEvent, IpcMessage};
use serde_json::json;

use common::{DaemonHandle, TempTestDir};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const PLAN_TEXT: &str = "# Build a CLI Todo App\n\n\
    ## Description\n\
    Create a Rust CLI application for managing todo items with SQLite persistence.\n\n\
    ## Acceptance Criteria\n\
    - Add, list, and complete todo items\n\
    - Persist data in a local SQLite database";

/// Drain events from the client until a matching event is found or timeout.
async fn wait_for_event(client: &mut IpcClient, event_name: &str, timeout: Duration) -> Result<DaemonEvent> {
    tokio::time::timeout(timeout, async {
        loop {
            match client.recv().await? {
                Some(IpcMessage::Event(e)) if e.event == event_name => return Ok(e),
                Some(_) => continue,
                None => return Err(eyre!("daemon disconnected while waiting for {event_name}")),
            }
        }
    })
    .await
    .map_err(|_| eyre!("timed out waiting for event: {event_name}"))?
}

// ---------------------------------------------------------------------------
// Bridge tests
// ---------------------------------------------------------------------------

/// accept_plan with raw plan text creates a Plan record with Active status
/// and the title extracted from the first line.
#[tokio::test]
async fn test_bridge_accept_plan_creates_plan() {
    let temp = TempTestDir::new("loopr-bridge");
    let daemon = DaemonHandle::spawn(temp).await.unwrap();
    let mut client = IpcClient::connect(&daemon.socket_path).await.unwrap();

    let (resp, _) = client
        .request("coordinator.accept_plan", json!({ "plan": PLAN_TEXT }))
        .await
        .unwrap();
    assert!(!resp.is_error(), "accept_plan failed: {:?}", resp.error);

    let result = resp.result.unwrap();
    let plan_id = result["plan_id"].as_str().unwrap();
    assert!(!plan_id.is_empty());

    // Verify Plan via plan.get
    let (get_resp, _) = client.request("plan.get", json!({ "id": plan_id })).await.unwrap();
    assert!(!get_resp.is_error(), "plan.get failed: {:?}", get_resp.error);

    let plan = get_resp.result.unwrap();
    assert_eq!(plan["status"].as_str().unwrap(), "active");
    assert_eq!(plan["title"].as_str().unwrap(), "# Build a CLI Todo App");
}

/// accept_plan creates a CoordinatorGoal record that is active and uses
/// the plan title as the goal text.
#[tokio::test]
async fn test_bridge_accept_plan_creates_goal() {
    let temp = TempTestDir::new("loopr-bridge");
    let daemon = DaemonHandle::spawn(temp).await.unwrap();
    let mut client = IpcClient::connect(&daemon.socket_path).await.unwrap();

    let (resp, _) = client
        .request("coordinator.accept_plan", json!({ "plan": PLAN_TEXT }))
        .await
        .unwrap();
    assert!(!resp.is_error(), "accept_plan failed: {:?}", resp.error);

    let result = resp.result.unwrap();
    let goal_id = result["goal_id"].as_str().unwrap();
    assert!(!goal_id.is_empty());

    // Verify active goal via coordinator.get_goal
    let (goal_resp, _) = client.request("coordinator.get_goal", json!({})).await.unwrap();
    assert!(!goal_resp.is_error(), "get_goal failed: {:?}", goal_resp.error);

    let goal = goal_resp.result.unwrap();
    assert_eq!(goal["id"].as_str().unwrap(), goal_id);
    assert!(goal["active"].as_bool().unwrap());
    assert_eq!(goal["goal"].as_str().unwrap(), "# Build a CLI Todo App");
}

/// accept_plan dispatches agent.start for the Coordinator, which emits
/// an agent.status_changed event with status "starting" before the async
/// task spawns. The Coordinator task may subsequently fail (no API key)
/// but we only verify the bridge wiring here.
#[tokio::test]
async fn test_bridge_accept_plan_starts_coordinator() {
    let temp = TempTestDir::new("loopr-bridge");
    let daemon = DaemonHandle::spawn(temp).await.unwrap();
    let mut client = IpcClient::connect(&daemon.socket_path).await.unwrap();

    // Subscribe before sending so we don't miss the event
    let (resp, events) = client
        .request("coordinator.accept_plan", json!({ "plan": PLAN_TEXT }))
        .await
        .unwrap();
    assert!(!resp.is_error(), "accept_plan failed: {:?}", resp.error);

    let result = resp.result.unwrap();
    assert!(result["accepted"].as_bool().unwrap());
    // coordinator_already_running=false means agent.start succeeded
    assert!(!result["coordinator_already_running"].as_bool().unwrap());

    // Verify agent.status_changed event was emitted with status "starting"
    let found_in_buffered = events
        .iter()
        .any(|e| e.event == "agent.status_changed" && e.data["status"].as_str() == Some("starting"));

    if !found_in_buffered {
        let event = wait_for_event(&mut client, "agent.status_changed", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(event.data["status"].as_str().unwrap(), "starting");
    }
}

/// accept_plan with an existing plan_id (from plan.create) activates that
/// plan, creates a goal, and starts the Coordinator.
#[tokio::test]
async fn test_bridge_accept_plan_with_existing_plan() {
    let temp = TempTestDir::new("loopr-bridge");
    let daemon = DaemonHandle::spawn(temp).await.unwrap();
    let mut client = IpcClient::connect(&daemon.socket_path).await.unwrap();

    // Create a plan via plan.create (starts as Draft)
    let (create_resp, _) = client
        .request(
            "plan.create",
            json!({
                "title": "Pre-existing Plan",
                "description": "A plan created before accept_plan",
                "acceptance_criteria": "Tests pass",
            }),
        )
        .await
        .unwrap();
    assert!(!create_resp.is_error(), "plan.create failed: {:?}", create_resp.error);

    let created = create_resp.result.unwrap();
    let plan_id = created["id"].as_str().unwrap();
    assert_eq!(created["status"].as_str().unwrap(), "draft");

    // Accept with plan_id
    let (accept_resp, _) = client
        .request("coordinator.accept_plan", json!({ "plan_id": plan_id }))
        .await
        .unwrap();
    assert!(!accept_resp.is_error(), "accept_plan failed: {:?}", accept_resp.error);

    let result = accept_resp.result.unwrap();
    assert_eq!(result["plan_id"].as_str().unwrap(), plan_id);
    assert!(!result["goal_id"].as_str().unwrap().is_empty());

    // Verify plan is now Active
    let (get_resp, _) = client.request("plan.get", json!({ "id": plan_id })).await.unwrap();
    let plan = get_resp.result.unwrap();
    assert_eq!(plan["status"].as_str().unwrap(), "active");

    // Verify Coordinator was started (coordinator_already_running=false means success)
    assert!(!result["coordinator_already_running"].as_bool().unwrap());
}

/// When a prior active goal exists, accept_plan deactivates it and
/// creates a new active goal for the new plan.
#[tokio::test]
async fn test_bridge_accept_plan_deactivates_prior_goal() {
    let temp = TempTestDir::new("loopr-bridge");
    let daemon = DaemonHandle::spawn(temp).await.unwrap();
    let mut client = IpcClient::connect(&daemon.socket_path).await.unwrap();

    // Set an initial goal via coordinator.set_goal
    let (goal_resp, _) = client
        .request("coordinator.set_goal", json!({ "goal": "First goal" }))
        .await
        .unwrap();
    assert!(!goal_resp.is_error(), "set_goal failed: {:?}", goal_resp.error);

    let first_goal_id = goal_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

    // Verify first goal is active
    let (check_resp, _) = client.request("coordinator.get_goal", json!({})).await.unwrap();
    assert_eq!(
        check_resp.result.as_ref().unwrap()["id"].as_str().unwrap(),
        first_goal_id,
    );

    // Accept a new plan - should deactivate the prior goal
    let (accept_resp, _) = client
        .request("coordinator.accept_plan", json!({ "plan": PLAN_TEXT }))
        .await
        .unwrap();
    assert!(!accept_resp.is_error(), "accept_plan failed: {:?}", accept_resp.error);

    let result = accept_resp.result.unwrap();
    let new_goal_id = result["goal_id"].as_str().unwrap();
    assert_ne!(new_goal_id, first_goal_id, "new goal should have a different ID");

    // Verify the active goal is now the new one
    let (new_check_resp, _) = client.request("coordinator.get_goal", json!({})).await.unwrap();
    let active_goal = new_check_resp.result.unwrap();
    assert_eq!(active_goal["id"].as_str().unwrap(), new_goal_id);
    assert!(active_goal["active"].as_bool().unwrap());
}
