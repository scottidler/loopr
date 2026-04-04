#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use serde_json::json;

use crate::agents::{AgentKind, AgentSession};
use crate::config::InterviewMode;
use crate::daemon::context::Stores;
use crate::test_util::TestDir;
use crate::worktree::manager::WorktreeManager;

use super::fixtures::*;

#[test]
fn test_coordinator_state_summary_multi_record() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create hierarchy first so work.create can find a valid phase
    let (_plan_id, _spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

    // Create work item under the real phase
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "WI-1", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
    );

    // Add agent session
    let session = AgentSession::new(AgentKind::Implementer, "model".into());
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session);

    // Build state summary (used by Coordinator to understand current state)
    let _agent_dir = TestDir::new("loopr-intg-logger");
    let agent_log = test_agent_logger(&_agent_dir);
    let summary = crate::agents::coordinator::build_state_summary(&stores, &agent_log);
    assert!(summary.contains("Test Plan"), "summary should include plan");
    assert!(summary.contains("WI-1"), "summary should include work item");
    assert!(summary.contains("implementer"), "summary should include agent session");
    assert!(
        summary.contains("### Active Agents"),
        "summary should have agents section"
    );
}

#[test]
fn test_coordinator_get_goal_full_lifecycle() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // No goal yet
    let r1 = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.get_goal", json!({}));
    assert_eq!(r1["active"], false);

    // Set goal
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "coordinator.set_goal",
        json!({"goal": "Build a website"}),
    );

    // Get goal - should return the active one
    let r2 = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.get_goal", json!({}));
    assert_eq!(r2["goal"], "Build a website");
    assert_eq!(r2["active"], true);

    // Clear goal
    dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.clear_goal", json!({}));

    // Get goal - should be inactive now
    let r3 = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.get_goal", json!({}));
    assert_eq!(r3["active"], false);
}

#[test]
fn test_coordinator_state_persistence_across_iterations() {
    use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Set a goal
    let goal = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "coordinator.set_goal",
        json!({"goal": "Build a REST API"}),
    );
    let goal_id = goal["id"].as_str().unwrap().to_string();

    // Create state, advance to Executing
    let mut state = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
    state.transition_to(CoordinatorFsmState::ActivatePhase);
    state.activate_phase("phase-1".to_string());
    state.increment_attempts("wi-1");
    state.increment_attempts("wi-1");
    let state_id = state.id.clone();

    // Persist to stores
    stores
        .coordinator_states
        .write()
        .unwrap()
        .insert(state_id.clone(), state);

    // Serialize from stores, deserialize - simulate restart
    let serialized = {
        let states = stores.coordinator_states.read().unwrap();
        let s = states.get(&state_id).unwrap();
        serde_json::to_string(s).unwrap()
    };

    let deserialized: CoordinatorState = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.id, state_id);
    assert_eq!(deserialized.goal_id, goal_id);
    assert_eq!(deserialized.fsm_state, CoordinatorFsmState::Executing);
    assert_eq!(deserialized.current_phase_id.as_deref(), Some("phase-1"));
    assert_eq!(deserialized.attempts("wi-1"), 2);
    assert!(deserialized.phase_activated_at.is_some());
}

#[tokio::test]
async fn test_coordinator_assigns_implementer_completes() {
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::implementer::{self, IterationOutcome, LlmClient};
    use async_trait::async_trait;
    use eyre::Result;

    let dir = TestDir::new("loopr-e2e-pipeline");

    // Init git repo so worktree code doesn't fail
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&dir)
        .output()
        .unwrap();

    // Build stores with repo_path set BEFORE wrapping in Arc
    let mut raw_stores = Stores::new();
    raw_stores.config.project.repo_path = dir.to_path_buf();
    let stores = Arc::new(raw_stores);

    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create hierarchy via IPC
    let (_plan_id, _spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

    // Create work item
    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent_id": phase_id, "title": "Write hello.txt", "description": "Create hello.txt with content", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
    );
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // Set coordinator goal
    use crate::domain::coordinator_goal::CoordinatorGoal;
    let goal = CoordinatorGoal::new("Build a hello world project".to_string());
    stores.coordinator_goals.write().unwrap().insert(goal.id.clone(), goal);

    // Verify work item starts as Ready (auto-promoted from Draft since acceptance_criteria present)
    let wi_resp = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_id}));
    assert_eq!(wi_resp["status"].as_str().unwrap(), "Ready");

    // Execute AssignAgent - should auto-transition Ready->InProgress
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
    let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), worktree_mgr, stores.config.clone());

    let assign_action = crate::agents::AgentAction::AssignAgent {
        agent_type: "implementer".to_string(),
        target_id: wi_id.clone(),
    };
    let agent_log = test_agent_logger(&dir);
    let assign_ctx = test_agent_context(&stores, bridge, tx.clone(), agent_log);
    let _ = crate::agents::executor::execute_action(&assign_action, &assign_ctx, &dir, None).await;

    // Verify work item is now InProgress
    let wi_resp = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_id}));
    assert_eq!(
        wi_resp["status"].as_str().unwrap(),
        "InProgress",
        "work item should be InProgress after auto-transition"
    );

    // Now run an implementer iteration directly with a mock LLM that writes a file
    struct PipelineLlm;

    #[async_trait]
    impl LlmClient for PipelineLlm {
        async fn call(&self, _system: &str, user_msg: &str) -> Result<String> {
            // Verify context includes goal and hierarchy
            assert!(user_msg.contains("Project Goal"), "missing Project Goal in context");
            assert!(user_msg.contains("hello world"), "missing goal text in context");
            assert!(user_msg.contains("Write hello.txt"), "missing work item in context");

            Ok(r#"[
                {"action": "write_file", "path": "hello.txt", "content": "Hello, World!"},
                {"action": "done", "summary": "Created hello.txt"}
            ]"#
            .to_string())
        }
    }

    let llm: Box<dyn LlmClient> = Box::new(PipelineLlm);
    let log_file_path = dir.join("test-pipeline.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
        .unwrap();
    let agent_log = crate::agents::agent_logger::AgentLogger::_new_for_test(
        AgentKind::Implementer,
        "test-pipeline",
        log_file,
        log_file_path,
    );
    let config = crate::config::AgentRoleConfig::default_implementer();
    let mut session = crate::agents::AgentSession::new(AgentKind::Implementer, "test".into());
    session.work_id = Some(wi_id.clone());
    session.worktree_path = Some(dir.to_string_lossy().into());
    let impl_bridge = crate::agents::bridge::AgentIpcBridge::new(
        stores.clone(),
        tx.clone(),
        WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees")),
        stores.config.clone(),
    );
    let ctx = crate::agents::AgentContext {
        session,
        stores: stores.clone(),
        bridge: impl_bridge,
        event_tx: tx.clone(),
        tool_runner: stores.read_tool_runner().unwrap(),
        tool_executor: stores.read_tool_executor().unwrap(),
        log: agent_log,
        read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
    };
    let agent = implementer::ImplementerAgent::new(ctx, llm, config, wi_id.clone(), dir.to_path_buf());

    let outcome = agent
        .run_iteration(1, None, &mut crate::agents::lifeguard::Lifeguard::new())
        .await
        .unwrap();
    assert!(
        matches!(outcome, IterationOutcome::Done(ref s) if s.contains("hello.txt")),
        "expected Done with hello.txt, got: {:?}",
        outcome
    );

    // Verify file was written
    let content = std::fs::read_to_string(dir.join("hello.txt")).unwrap();
    assert_eq!(content, "Hello, World!");
}
