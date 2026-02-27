use std::path::Path;
use std::sync::Arc;

use eyre::{Result, eyre};
use log::{error, info, warn};
use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::coordinator;
use crate::agents::implementer::{self, LlmClient};
use crate::agents::llm_client::AgentLlmClient;
use crate::agents::reviewer;
use crate::agents::{AgentAction, AgentSession, AgentStatus, AgentType};
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::{ToolResult, ToolRunner};
use crate::worktree::manager::WorktreeManager;

/// Create the LLM client. Fails if the API key env var is not set.
fn create_llm_client(
    config: &crate::config::AgentRoleConfig,
    session_id: &str,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<Box<dyn LlmClient>> {
    let client = AgentLlmClient::new(config.clone(), session_id.to_string(), event_tx.clone())?;
    info!("Agent {} using LLM client (model: {})", session_id, config.model);
    Ok(Box::new(client))
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

    // Create a worktree for the agent before starting the loop.
    // Thinking plane agents (Coordinator, Researcher, Integrator) don't use worktrees.
    // Implementers key on work_item_id, Reviewers on bundle_id.
    let worktree_key = if agent_type.is_thinking_plane() {
        None
    } else {
        let sessions = stores.agent_sessions.read().unwrap();
        let session = match sessions.get(&session_id) {
            Some(s) => s,
            None => {
                error!("Agent {} session not found in stores", session_id);
                return;
            }
        };
        match agent_type {
            AgentType::Implementer => session.work_item_id.clone(),
            AgentType::Reviewer => session.bundle_id.clone(),
            // Already handled by is_thinking_plane() above
            AgentType::Coordinator | AgentType::Researcher | AgentType::Integrator => None,
        }
    };

    if let Some(ref key) = worktree_key {
        let worktree_path = match worktree_mgr.create(key, "HEAD") {
            Ok(path) => Some(path),
            Err(crate::worktree::manager::WorktreeError::AlreadyExists(_)) => {
                info!("Agent {} worktree already exists for {}", session_id, key);
                Some(worktree_mgr.worktree_path(key))
            }
            Err(e) => {
                warn!("Agent {} failed to create worktree: {}", session_id, e);
                None
            }
        };

        if let Some(ref path) = worktree_path {
            let mut sessions = stores.agent_sessions.write().unwrap();
            if let Some(session) = sessions.get_mut(&session_id) {
                session.worktree_path = Some(path.to_string_lossy().to_string());
                persist_session(&stores, session);
            }
            info!("Agent {} worktree created at {}", session_id, path.display());
        }
    }

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

    match agent_type {
        AgentType::Implementer => {
            let tool_runner = &stores.tool_runner;
            let config = stores.config.agents.implementer.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;

            // Clone session out for the implementer loop
            let mut session = {
                let sessions = stores.agent_sessions.read().unwrap();
                sessions
                    .get(session_id)
                    .ok_or_else(|| eyre!("session not found: {}", session_id))?
                    .clone()
            };

            let result = implementer::run_implementer(
                llm.as_ref(),
                &mut session,
                stores,
                tool_runner,
                bridge,
                &config,
                event_tx,
            )
            .await;

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
            let config = stores.config.agents.reviewer.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;

            // Clone session out for the reviewer loop
            let mut session = {
                let sessions = stores.agent_sessions.read().unwrap();
                sessions
                    .get(session_id)
                    .ok_or_else(|| eyre!("session not found: {}", session_id))?
                    .clone()
            };

            let result = reviewer::run_reviewer(llm.as_ref(), &mut session, stores, bridge, &config, event_tx).await;

            // Write back updated session iteration count
            {
                let mut sessions = stores.agent_sessions.write().unwrap();
                if let Some(s) = sessions.get_mut(session_id) {
                    s.iteration = session.iteration;
                }
            }

            result
        }
        AgentType::Coordinator => {
            let config = stores.config.agents.coordinator.clone();
            let llm = create_llm_client(&config.role, session_id, event_tx)?;

            let mut session = {
                let sessions = stores.agent_sessions.read().unwrap();
                sessions
                    .get(session_id)
                    .ok_or_else(|| eyre!("session not found: {}", session_id))?
                    .clone()
            };

            let result =
                coordinator::run_coordinator(llm.as_ref(), &mut session, stores, bridge, &config, event_tx).await;

            // Write back updated session iteration count
            {
                let mut sessions = stores.agent_sessions.write().unwrap();
                if let Some(s) = sessions.get_mut(session_id) {
                    s.iteration = session.iteration;
                }
            }

            result
        }
        AgentType::Researcher => Err(eyre!("Researcher agent loop not yet implemented")),
        AgentType::Integrator => Err(eyre!("Integrator task loop not yet implemented")),
    }
}

/// Execute a single agent action. Used by the agent loop to process parsed LLM responses.
///
/// `agent_type` is used for role inference on Transition actions (when role is None).
pub async fn execute_action(
    action: &AgentAction,
    tool_runner: &ToolRunner,
    bridge: &AgentIpcBridge,
    worktree_path: &Path,
    work_item_id: Option<&str>,
    agent_type: AgentType,
) -> Result<ActionResult> {
    match action {
        AgentAction::RunTool { tool_name, args } => {
            let tool_result = tool_runner
                .run(tool_name, args, worktree_path)
                .await
                .map_err(|e| eyre!("run_tool '{}': {}", tool_name, e))?;
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
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| eyre!("write_file mkdir '{}': {}", path, e))?;
            }
            tokio::fs::write(&full_path, content)
                .await
                .map_err(|e| eyre!("write_file '{}': {}", path, e))?;
            Ok(ActionResult::FileWritten(path.clone()))
        }
        AgentAction::ReadFile { path } => {
            let full_path = worktree_path.join(path);
            let content = tokio::fs::read_to_string(&full_path)
                .await
                .map_err(|e| eyre!("read_file '{}': {}", path, e))?;
            Ok(ActionResult::FileRead(content))
        }
        AgentAction::Commit { message, paths } => {
            // Stage specified paths (or all changes if empty)
            let add_args = if paths.is_empty() { vec!["-A".to_string()] } else { paths.clone() };
            let mut add_cmd = tokio::process::Command::new("git");
            add_cmd.arg("add").args(&add_args).current_dir(worktree_path);
            let add_out = add_cmd.output().await?;
            if !add_out.status.success() {
                let stderr = String::from_utf8_lossy(&add_out.stderr);
                return Err(eyre!("git add failed: {}", stderr));
            }

            // Commit
            let mut commit_cmd = tokio::process::Command::new("git");
            commit_cmd.args(["commit", "-m", message]).current_dir(worktree_path);
            let commit_out = commit_cmd.output().await?;
            if !commit_out.status.success() {
                let stderr = String::from_utf8_lossy(&commit_out.stderr);
                return Err(eyre!("git commit failed: {}", stderr));
            }
            Ok(ActionResult::Committed(message.clone()))
        }
        AgentAction::ProposeBundle { description, claims } => {
            // Get the current branch name from the worktree
            let mut branch_cmd = tokio::process::Command::new("git");
            branch_cmd
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(worktree_path);
            let branch_out = branch_cmd.output().await?;
            let branch_name = String::from_utf8_lossy(&branch_out.stdout).trim().to_string();

            let wi_id = work_item_id.ok_or_else(|| eyre!("propose_bundle requires work_item_id"))?;
            let resp = bridge.request(
                "bundle.create",
                serde_json::json!({
                    "work_item_id": wi_id,
                    "branch_name": branch_name,
                    "claims": claims,
                }),
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
            role,
        } => {
            // If role is not specified, infer from agent_type
            let effective_role = role
                .as_ref()
                .map(|r| r.to_string())
                .unwrap_or_else(|| agent_type.default_role().to_string());
            let params = serde_json::json!({ "id": id, "target": target_state, "role": effective_role });
            let method = format!("{}.transition", collection);
            let resp = bridge.request(&method, params);
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
            applicable_roles,
            resource_tags,
        } => {
            let mut params = serde_json::json!({
                "content": content,
                "scope": scope,
                "source_id": source_id,
            });
            if let Some(roles) = applicable_roles {
                params["applicable_roles"] = serde_json::json!(roles);
            }
            if let Some(tags) = resource_tags {
                params["resource_tags"] = serde_json::json!(tags);
            }
            let resp = bridge.request("learning.create", params);
            if resp.is_error() {
                return Err(eyre!("create learning failed: {:?}", resp.error));
            }
            Ok(ActionResult::LearningCreated(content.clone()))
        }
        AgentAction::Done { summary } => Ok(ActionResult::Done(summary.clone())),
        AgentAction::NeedHelp { reason } => Ok(ActionResult::NeedHelp(reason.clone())),

        // --- Coordinator actions (stubs — wired in Phase 2 coordinator.rs) ---
        AgentAction::CreatePlan { title, .. } => Ok(ActionResult::NotYetImplemented(format!("CreatePlan: {}", title))),
        AgentAction::CreateSpec { title, .. } => Ok(ActionResult::NotYetImplemented(format!("CreateSpec: {}", title))),
        AgentAction::CreatePhase { title, .. } => {
            Ok(ActionResult::NotYetImplemented(format!("CreatePhase: {}", title)))
        }
        AgentAction::CreateWorkItem { title, .. } => {
            Ok(ActionResult::NotYetImplemented(format!("CreateWorkItem: {}", title)))
        }
        AgentAction::AssignAgent { agent_type, target_id } => Ok(ActionResult::NotYetImplemented(format!(
            "AssignAgent: {} → {}",
            agent_type, target_id
        ))),
        AgentAction::SpawnResearcher { query, scope_id } => Ok(ActionResult::NotYetImplemented(format!(
            "SpawnResearcher: {} (scope: {})",
            query, scope_id
        ))),
        AgentAction::ValidateDocument { collection, id } => Ok(ActionResult::NotYetImplemented(format!(
            "ValidateDocument: {}/{}",
            collection, id
        ))),
        AgentAction::AcquireLock { resource, holder_id } => {
            // Check if there's already an active lock on this resource
            let check_resp = bridge.request(
                "lock.list",
                serde_json::json!({ "resource": resource, "active_only": true }),
            );
            if check_resp.is_error() {
                return Err(eyre!("lock.list failed: {:?}", check_resp.error));
            }
            if let Some(result) = &check_resp.result
                && let Some(locks) = result.as_array()
                && !locks.is_empty()
            {
                // Resource already locked — return as ActionError so the LLM can self-correct
                let existing_holder = locks[0]
                    .get("holder_id")
                    .and_then(|v: &serde_json::Value| v.as_str())
                    .unwrap_or("unknown");
                let existing_id = locks[0]
                    .get("id")
                    .and_then(|v: &serde_json::Value| v.as_str())
                    .unwrap_or("unknown");
                return Ok(ActionResult::ActionError(format!(
                    "resource '{}' already locked by {} (lock_id: {})",
                    resource, existing_holder, existing_id
                )));
            }

            // Create the lock — granted_by is the holder_id (self-granted by coordinator)
            let resp = bridge.request(
                "lock.create",
                serde_json::json!({
                    "resource": resource,
                    "holder_id": holder_id,
                    "granted_by": holder_id,
                }),
            );
            if resp.is_error() {
                return Err(eyre!("lock.create failed: {:?}", resp.error));
            }
            let lock_id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            info!("Lock acquired: {} on '{}' for {}", lock_id, resource, holder_id);
            Ok(ActionResult::LockAcquired(lock_id))
        }
        AgentAction::ReleaseLock { lock_id } => {
            let resp = bridge.request("lock.release", serde_json::json!({ "id": lock_id }));
            if resp.is_error() {
                return Err(eyre!("lock.release failed: {:?}", resp.error));
            }
            info!("Lock released: {}", lock_id);
            Ok(ActionResult::LockReleased(lock_id.clone()))
        }
        AgentAction::TriageBundle { bundle_id } => {
            Ok(ActionResult::NotYetImplemented(format!("TriageBundle: {}", bundle_id)))
        }
        AgentAction::AcceptBundle { bundle_id } => {
            Ok(ActionResult::NotYetImplemented(format!("AcceptBundle: {}", bundle_id)))
        }

        // --- Researcher actions (stubs — wired in Phase 4 researcher.rs) ---
        AgentAction::SearchCode { pattern, .. } => {
            Ok(ActionResult::NotYetImplemented(format!("SearchCode: {}", pattern)))
        }
        AgentAction::SearchFiles { pattern, .. } => {
            Ok(ActionResult::NotYetImplemented(format!("SearchFiles: {}", pattern)))
        }
        AgentAction::ListDirectory { path } => Ok(ActionResult::NotYetImplemented(format!("ListDirectory: {}", path))),
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
    /// Lock acquired — contains lock_id.
    LockAcquired(String),
    /// Lock released — contains lock_id.
    LockReleased(String),
    Done(String),
    NeedHelp(String),
    /// Non-fatal error — fed back to the LLM so it can self-correct.
    ActionError(String),
    /// Action type recognized but execution not yet implemented.
    /// Used for Coordinator/Researcher actions during incremental build.
    NotYetImplemented(String),
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
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
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
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
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
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
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
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
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
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
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
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_action_acquire_lock() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-acqlock-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-123".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::LockAcquired(lock_id) = &result {
            assert!(!lock_id.is_empty());
            assert_ne!(lock_id, "unknown");
        } else {
            panic!("expected LockAcquired result, got {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_action_acquire_lock_conflict() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-lockconf-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        // Acquire first lock
        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-100".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        assert!(matches!(result, ActionResult::LockAcquired(_)));

        // Try to acquire again on same resource — should get ActionError
        let action2 = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-200".to_string(),
        };
        let result2 = execute_action(&action2, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::ActionError(msg) = &result2 {
            assert!(msg.contains("already locked"), "expected conflict message, got: {}", msg);
        } else {
            panic!("expected ActionError for lock conflict, got {:?}", result2);
        }
    }

    #[tokio::test]
    async fn test_execute_action_release_lock() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-rellock-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        // Acquire a lock first
        let acquire_action = AgentAction::AcquireLock {
            resource: "src/lib.rs".to_string(),
            holder_id: "wi-456".to_string(),
        };
        let acquire_result = execute_action(&acquire_action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = acquire_result {
            id
        } else {
            panic!("expected LockAcquired");
        };

        // Release it
        let release_action = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        let release_result = execute_action(&release_action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::LockReleased(id) = &release_result {
            assert_eq!(id, &lock_id);
        } else {
            panic!("expected LockReleased result, got {:?}", release_result);
        }
    }

    #[tokio::test]
    async fn test_execute_action_acquire_after_release() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-reacq-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, Config::default());

        // Acquire
        let action1 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-1".to_string(),
        };
        let r1 = execute_action(&action1, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = r1 {
            id
        } else {
            panic!("expected LockAcquired");
        };

        // Release
        let release = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        execute_action(&release, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();

        // Re-acquire same resource by different holder — should succeed
        let action2 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-2".to_string(),
        };
        let r2 = execute_action(&action2, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        assert!(matches!(r2, ActionResult::LockAcquired(_)));
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
