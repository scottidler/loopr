//! Phase 8 end-to-end integration tests for the Director pipeline.
//!
//! These exercise the IPC dispatch surface end-to-end. They deliberately do not
//! spawn a real Director agent task (that requires a live LLM). Instead each test
//! walks a representative slice of the Chat → /plan → Director → corrective-action
//! pipeline through the handler layer, which is what the design doc covers.
//!
//! Full-stack LLM-driven smoke tests belong in `bin/e2e/` against a target repo.

#![allow(clippy::unwrap_used, unused_imports)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::director::actions::{DirectorAction, ReDecomposeTarget, execute_actions};
use crate::agents::{AgentKind, AgentSession, AgentStatus};
use crate::domain::chat::{ChatHistory, FunnelState};
use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::phase::Phase;
use crate::domain::plan::{HierarchyStatus, Plan};
use crate::domain::spec::Spec;
use crate::worktree::manager::WorktreeManager;

use super::fixtures::*;

/// Phase 8 / E2E #1:
/// Walks Chat → /plan → Director handoff through IPC dispatch:
///   1. `chat.submit` seeds a chat history,
///   2. seed a Director session + mpsc sender (simulating what `director.start_plan_intake`
///      would do, minus the task spawn we can't drive without a live LLM),
///   3. a subsequent `chat.submit` for the same session routes via `director_message_tx`
///      instead of spinning up a new Chat agent (the "routed_to: director" path).
///
/// This validates the cross-handler pipeline the design doc mandates: once the Director
/// owns the conversation, user chat messages land in its mpsc channel.
#[tokio::test]
async fn test_director_pipeline_chat_to_director_handoff_routes_messages() {
    crate::prompts::init_defaults();

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let chat_session_id = "default-chat";

    // 1. First chat.submit seeds the chat history (bypasses Director).
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "chat.submit",
        json!({
            "session-id": chat_session_id,
            "message": "I want to refactor the logger",
            "funnel-state": "chat",
        }),
    )
    .await;

    // 2. Simulate director.start_plan_intake wiring: stash the Director session and mpsc
    //    sender, link the chat history. We skip the real run_agent_task spawn because it
    //    requires a live LLM.
    let dir_session = AgentSession::new(AgentKind::Director, "test-model".into());
    let dir_sid = dir_session.id.clone();
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(dir_sid.clone(), dir_session);

    let (msg_tx, mut msg_rx) = mpsc::channel::<String>(4);
    stores
        .director_message_tx
        .write()
        .unwrap()
        .insert(dir_sid.clone(), msg_tx);
    stores
        .chat_sessions
        .write()
        .unwrap()
        .get_mut(chat_session_id)
        .unwrap()
        .director_session_id = Some(dir_sid.clone());

    // 3. Next chat.submit must route to the Director mpsc, not a Chat loop.
    let routed = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "chat.submit",
        json!({
            "session-id": chat_session_id,
            "message": "please re-decompose phase 2",
            "funnel-state": "executing",
        }),
    )
    .await;

    assert_eq!(
        routed["routed_to"].as_str(),
        Some("director"),
        "post-handoff chat must route to Director"
    );
    assert_eq!(routed["director_session_id"].as_str(), Some(dir_sid.as_str()));

    let forwarded = msg_rx.try_recv().expect("message must arrive on Director mpsc");
    assert_eq!(forwarded, "please re-decompose phase 2");
}

/// Phase 8 / E2E #2:
/// Director escalation on broken AC: the Director's corrective action for a stuck Phase
/// is `re-decompose`, which transitions the parent Phase back to Draft so the engine
/// re-runs the decomposer. This test runs that action through the real bridge + FSM
/// + transition handler, the same path `execute_actions` follows in production.
///
/// Verifies:
///   - a Director-role override from Active → Draft on Phase is accepted,
///   - the Phase persists in Draft,
///   - `director.action_taken` is emitted for the audit trail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_director_escalation_redecompose_action_pipeline() {
    let dir = crate::test_util::TestDir::new("director-integration-redecompose");
    let stores = test_stores_with_persistence(&dir);
    let (event_tx, mut rx) = broadcast::channel::<crate::ipc::protocol::DaemonEvent>(64);

    // Seed a Plan → Spec → Phase hierarchy that looks "stuck" in Active.
    let mut plan = Plan::new("esc-plan".into(), AcceptanceCriteria::default());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "esc-spec".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.write_specs().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id.clone(), "esc-phase".into());
    phase.force_status(HierarchyStatus::Active);
    let phase_id = phase.id.clone();
    stores.write_phases().unwrap().insert(phase_id.clone(), phase);

    let worktree_mgr = WorktreeManager::new(
        stores.config.project.repo_path.clone(),
        stores.config.project.repo_path.join(".worktrees"),
    );
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );

    let action = DirectorAction::ReDecompose {
        target_type: ReDecomposeTarget::Phase,
        target_id: phase_id.clone(),
        reason: Some("AC has no assertable bullets".into()),
    };
    let report = tokio::task::spawn_blocking({
        let event_tx = event_tx.clone();
        move || execute_actions(&[action], &bridge, &event_tx, "director-integration-test")
    })
    .await
    .unwrap();

    assert_eq!(
        report.ok, 1,
        "re-decompose should succeed; details={:?}",
        report.details
    );
    assert_eq!(report.failed, 0);

    // Phase ended up in Draft so the engine would pick it up and re-run decomposition.
    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&phase_id).unwrap().status(), HierarchyStatus::Draft);

    // Audit-trail event was emitted.
    let mut saw_action_taken = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "director.action_taken" && ev.data.get("action").and_then(|v| v.as_str()) == Some("re-decompose")
        {
            saw_action_taken = true;
        }
    }
    assert!(saw_action_taken, "re-decompose must emit director.action_taken");
}

/// Phase 8 / E2E #3:
/// User intervention routing: a `director.user_message` IPC call delivers the message
/// to the Director's mpsc channel and emits the corresponding broadcast event so the
/// TUI can render it in the audit trail. This validates the IPC plumbing the Director
/// depends on during Monitoring → UserIntervention.
#[tokio::test]
async fn test_director_user_intervention_delivers_via_dispatch() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Seed an active Director session and wire its mpsc channel.
    let mut dir = AgentSession::new(AgentKind::Director, "test-model".into());
    let _ = dir.transition_to(AgentStatus::Running);
    let dir_sid = dir.id.clone();
    stores.agent_sessions.write().unwrap().insert(dir_sid.clone(), dir);

    let (msg_tx, mut msg_rx) = mpsc::channel::<String>(4);
    stores
        .director_message_tx
        .write()
        .unwrap()
        .insert(dir_sid.clone(), msg_tx);

    let mut bcast_rx = tx.subscribe();

    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "director.user_message",
        json!({
            "session-id": dir_sid,
            "message": "prioritize work wk-99 and abandon wk-100",
        }),
    )
    .await;

    let delivered = msg_rx.try_recv().expect("user message must reach the Director mpsc");
    assert_eq!(delivered, "prioritize work wk-99 and abandon wk-100");

    // Broadcast event lets the TUI render the intent in the audit trail.
    let mut saw_event = false;
    while let Ok(ev) = bcast_rx.try_recv() {
        if ev.event == "director.user_message"
            && ev.data.get("session-id").and_then(|v| v.as_str()) == Some(dir_sid.as_str())
        {
            saw_event = true;
        }
    }
    assert!(saw_event, "director.user_message broadcast must be emitted");
}
