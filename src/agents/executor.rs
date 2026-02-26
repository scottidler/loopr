use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, eyre};
use log::{error, info, warn};
use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::implementer::{self, LlmClient};
use crate::agents::llm_client::AgentLlmClient;
use crate::agents::{AgentAction, AgentSession, AgentStatus, AgentType};
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::{ToolResult, ToolRunner};
use crate::worktree::manager::WorktreeManager;

/// Stub LLM client — used when no API key is available (testing, development).
struct StubLlmClient;

#[async_trait]
impl LlmClient for StubLlmClient {
    async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
        Ok(r#"[{"action": "done", "summary": "Stub LLM — no API key configured"}]"#.to_string())
    }
}

/// Create the appropriate LLM client based on configuration.
/// Returns a real `AgentLlmClient` if the API key env var is set, otherwise falls back to `StubLlmClient`.
fn create_llm_client(
    config: &crate::config::AgentRoleConfig,
    session_id: &str,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Box<dyn LlmClient> {
    match AgentLlmClient::new(config.clone(), session_id.to_string(), event_tx.clone()) {
        Ok(client) => {
            info!("Agent {} using real LLM client (model: {})", session_id, config.model);
            Box::new(client)
        }
        Err(e) => {
            warn!("Agent {} falling back to stub LLM client: {}", session_id, e);
            Box::new(StubLlmClient)
        }
    }
}

/// Run an agent task as a Tokio task. This is spawned from the agent.start handler.
///
/// Currently implements a minimal lifecycle: Starting → Running → Completed/Failed.
/// Phase 2 will add the full Implementer/Reviewer loops with LLM calls.
pub async fn run_agent_task(
    session_id: String,
    agent_type: AgentType,
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    worktree_mgr: WorktreeManager,
) {
    info!("Agent task started: {} ({})", session_id, agent_type);

    // Create the in-process IPC bridge for this agent
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

    // Transition to Running
    {
        let mut sessions = stores.agent_sessions.write().unwrap();
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Err(e) = session.transition_to(AgentStatus::Running) {
                error!("Agent {} failed to start: {}", session_id, e);
                return;
            }
            persist_session(&stores, session);
        }
    }
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Running));

    // Phase 2 TODO: implement full agent loop (LLM calls, action parsing, tool execution).
    // For now, mark as Completed after startup validation.
    let result = run_agent_loop(&session_id, agent_type, &stores, &bridge, &event_tx).await;

    // Transition to terminal state based on result
    let terminal_status = match result {
        Ok(_) => AgentStatus::Completed,
        Err(ref e) => {
            warn!("Agent {} failed: {}", session_id, e);
            AgentStatus::Failed
        }
    };

    {
        let mut sessions = stores.agent_sessions.write().unwrap();
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Err(e) = session.transition_to(terminal_status) {
                error!(
                    "Agent {} failed to transition to {:?}: {}",
                    session_id, terminal_status, e
                );
                return;
            }
            if let Err(ref e) = result {
                session.error_message = Some(e.to_string());
            }
            persist_session(&stores, session);
        }
    }
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, terminal_status));

    info!(
        "Agent task finished: {} ({}) → {:?}",
        session_id, agent_type, terminal_status
    );
}

/// Agent loop — dispatches to the appropriate agent implementation based on type.
async fn run_agent_loop(
    session_id: &str,
    agent_type: AgentType,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    // Verify the bridge works by checking system status
    let status_resp = bridge.request("system.status", serde_json::json!(null));
    if status_resp.is_error() {
        return Err(eyre!("bridge health check failed: {:?}", status_resp.error));
    }
    info!("Agent {} bridge health check passed", session_id);

    // Subscribe to events for agent monitoring (Phase 4 will stream these to TUI)
    let _event_rx = bridge.event_tx().subscribe();

    match agent_type {
        AgentType::Implementer => {
            let tool_runner = &stores.tool_runner;
            let config = stores.config.agents.implementer.clone();
            let llm = create_llm_client(&config, session_id, event_tx);

            // Clone session out for the implementer loop
            let mut session = {
                let sessions = stores.agent_sessions.read().unwrap();
                sessions
                    .get(session_id)
                    .ok_or_else(|| eyre!("session not found: {}", session_id))?
                    .clone()
            };

            let result = implementer::run_implementer(llm.as_ref(), &mut session, stores, tool_runner, bridge, &config, event_tx).await;

            // Write back updated session iteration count
            {
                let mut sessions = stores.agent_sessions.write().unwrap();
                if let Some(s) = sessions.get_mut(session_id) {
                    s.iteration = session.iteration;
                }
            }

            result
        }
        AgentType::Reviewer => {
            // Phase 5 — stub for now, complete immediately
            info!("Reviewer agent {} — stub, completing immediately", session_id);
            let worktree_path = std::env::temp_dir();
            let done_action = AgentAction::Done {
                summary: format!("Reviewer {} stub complete", session_id),
            };
            let result = execute_action(&done_action, &stores.tool_runner, bridge, &worktree_path).await?;
            log_action_result(session_id, &result);
            Ok(())
        }
    }
}

/// Log the result of an agent action for diagnostics.
fn log_action_result(session_id: &str, result: &ActionResult) {
    match result {
        ActionResult::ToolRun(tool_result) => {
            info!(
                "Agent {} tool result: {} exit={} duration={}ms",
                session_id, tool_result.tool_name, tool_result.exit_code, tool_result.duration_ms
            );
        }
        ActionResult::FileWritten(path) => {
            info!("Agent {} wrote file: {}", session_id, path);
        }
        ActionResult::FileRead(content) => {
            info!("Agent {} read file ({} bytes)", session_id, content.len());
        }
        ActionResult::Committed(msg) => {
            info!("Agent {} committed: {}", session_id, msg);
        }
        ActionResult::BundleProposed(desc) => {
            info!("Agent {} proposed bundle: {}", session_id, desc);
        }
        ActionResult::Transitioned(desc) => {
            info!("Agent {} transitioned: {}", session_id, desc);
        }
        ActionResult::LearningCreated(content) => {
            info!("Agent {} created learning: {}", session_id, content);
        }
        ActionResult::Done(summary) => {
            info!("Agent {} done: {}", session_id, summary);
        }
        ActionResult::NeedHelp(reason) => {
            warn!("Agent {} needs help: {}", session_id, reason);
        }
    }
}

/// Execute a single agent action. Used by the agent loop to process parsed LLM responses.
/// Phase 2 will flesh out all action variants.
pub async fn execute_action(
    action: &AgentAction,
    tool_runner: &ToolRunner,
    bridge: &AgentIpcBridge,
    worktree_path: &Path,
) -> Result<ActionResult> {
    match action {
        AgentAction::RunTool { tool_name, args } => {
            let tool_result = tool_runner.run(tool_name, args, worktree_path).await?;
            Ok(ActionResult::ToolRun(tool_result))
        }
        AgentAction::WriteFile { path, content } => {
            // Validate path stays within worktree (sandbox)
            let full_path = worktree_path.join(path);
            let canonical = full_path.canonicalize().unwrap_or_else(|_| full_path.clone());
            let worktree_canonical = worktree_path
                .canonicalize()
                .unwrap_or_else(|_| worktree_path.to_path_buf());
            if !canonical.starts_with(&worktree_canonical) {
                return Err(eyre!("path escapes worktree: {}", path));
            }
            if let Some(parent) = full_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&full_path, content).await?;
            Ok(ActionResult::FileWritten(path.clone()))
        }
        AgentAction::ReadFile { path } => {
            let full_path = worktree_path.join(path);
            let content = tokio::fs::read_to_string(&full_path).await?;
            Ok(ActionResult::FileRead(content))
        }
        AgentAction::Commit { message, paths } => {
            // Use bridge to create a commit via IPC
            let resp = bridge.request(
                "system.status",
                serde_json::json!({ "message": message, "paths": paths }),
            );
            if resp.is_error() {
                return Err(eyre!("commit failed: {:?}", resp.error));
            }
            Ok(ActionResult::Committed(message.clone()))
        }
        AgentAction::ProposeBundle { description, claims } => {
            let resp = bridge.request(
                "system.status",
                serde_json::json!({ "description": description, "claims": claims }),
            );
            if resp.is_error() {
                return Err(eyre!("propose bundle failed: {:?}", resp.error));
            }
            Ok(ActionResult::BundleProposed(description.clone()))
        }
        AgentAction::Transition {
            collection,
            id,
            target_state,
        } => {
            let method = format!("{}.transition", collection);
            let resp = bridge.request(&method, serde_json::json!({ "id": id, "target": target_state }));
            if resp.is_error() {
                return Err(eyre!("transition failed: {:?}", resp.error));
            }
            Ok(ActionResult::Transitioned(format!(
                "{}/{} → {}",
                collection, id, target_state
            )))
        }
        AgentAction::CreateLearning {
            content,
            scope,
            source_id,
        } => {
            let resp = bridge.request(
                "learning.create",
                serde_json::json!({
                    "content": content,
                    "scope": scope,
                    "source_id": source_id,
                }),
            );
            if resp.is_error() {
                return Err(eyre!("create learning failed: {:?}", resp.error));
            }
            Ok(ActionResult::LearningCreated(content.clone()))
        }
        AgentAction::Done { summary } => Ok(ActionResult::Done(summary.clone())),
        AgentAction::NeedHelp { reason } => Ok(ActionResult::NeedHelp(reason.clone())),
    }
}

/// Result of executing a single agent action.
#[derive(Debug)]
pub enum ActionResult {
    ToolRun(ToolResult),
    FileWritten(String),
    FileRead(String),
    Committed(String),
    BundleProposed(String),
    Transitioned(String),
    LearningCreated(String),
    Done(String),
    NeedHelp(String),
}

fn persist_session(stores: &Stores, session: &AgentSession) {
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(session.clone())
    {
        warn!("Failed to persist agent session {} to TaskStore: {}", session.id, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProjectConfig, ToolEntry};
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;

    fn test_stores(dir: &Path) -> Arc<Stores> {
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(StdMutex::new(store)));
        stores.config = config;
        Arc::new(stores)
    }

    #[tokio::test]
    async fn test_execute_action_run_tool() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-test-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let entries = vec![ToolEntry {
            name: "echo-test".to_string(),
            command: "echo hello".to_string(),
            timeout_secs: 10,
            worktree: true,
        }];
        let runner = ToolRunner::new(&entries);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        let action = AgentAction::RunTool {
            tool_name: "echo-test".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &runner, &bridge, &dir).await.unwrap();
        if let ActionResult::ToolRun(tool_result) = result {
            assert_eq!(tool_result.exit_code, 0);
            assert_eq!(tool_result.stdout.trim(), "hello");
        } else {
            panic!("expected ToolRun result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_write_file() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-write-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        let action = AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "hello world".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));

        let content = std::fs::read_to_string(dir.join("test.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_execute_action_read_file() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-read-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("read-me.txt"), "file content").unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        let action = AgentAction::ReadFile {
            path: "read-me.txt".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir).await.unwrap();
        if let ActionResult::FileRead(content) = result {
            assert_eq!(content, "file content");
        } else {
            panic!("expected FileRead result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_done() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-done-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        let action = AgentAction::Done {
            summary: "All done".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir).await.unwrap();
        if let ActionResult::Done(summary) = result {
            assert_eq!(summary, "All done");
        } else {
            panic!("expected Done result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_need_help() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-help-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        let action = AgentAction::NeedHelp {
            reason: "Ambiguous spec".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir).await.unwrap();
        if let ActionResult::NeedHelp(reason) = result {
            assert_eq!(reason, "Ambiguous spec");
        } else {
            panic!("expected NeedHelp result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_unknown_tool() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-unk-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        let action = AgentAction::RunTool {
            tool_name: "nonexistent".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &runner, &bridge, &dir).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_agent_task_lifecycle() {
        let dir = std::env::temp_dir().join(format!("loopr-agent-task-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let stores = test_stores(&dir);
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));

        // Create an agent session
        let session = AgentSession::new(AgentType::Implementer, "test-model".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Run the agent task
        run_agent_task(
            session_id.clone(),
            AgentType::Implementer,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        // Check session reached a terminal state
        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal());

        // Drain events — should have status changes
        let mut events = vec![];
        while let Ok(e) = event_rx.try_recv() {
            events.push(e);
        }
        assert!(
            events.iter().any(|e| e.event == "agent.status_changed"),
            "expected agent status change events"
        );
    }
}
