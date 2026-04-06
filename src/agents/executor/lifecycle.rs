use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use tokio::sync::broadcast;
use tracing::{debug, error, info, instrument, warn};

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::{Agent, AgentContext, AgentKind, AgentStatus};
use crate::agents::{coordinator, implementer, integrator, researcher, reviewer};
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

use super::llm::create_llm_client;
use super::util::{determine_work_handback, persist_session, release_agent_locks, resolve_worktree_base};

/// Run an agent task as a Tokio task. This is spawned from the agent.start handler.
///
/// Currently implements a minimal lifecycle: Starting -> Running -> Completed/Failed.
/// Phase 2 will add the full Implementer/Reviewer loops with LLM calls.
#[instrument(skip_all, fields(session_id = %session_id, agent_type = %agent_type))]
pub async fn run_agent_task(
    session_id: String,
    agent_type: AgentKind,
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    worktree_mgr: WorktreeManager,
) {
    debug!("session_id={} agent_type={}", session_id, agent_type);
    info!("Agent task started: {} ({})", session_id, agent_type);

    // Create a worktree for the agent before starting the loop.
    // Thinking plane agents (Coordinator, Researcher, Integrator) don't use worktrees.
    // Implementers key on work_id, Reviewers on bundle_id.
    let worktree_key = if agent_type.is_thinking_plane() {
        None
    } else {
        let Ok(sessions) = stores.read_agent_sessions() else {
            error!("agent_sessions lock poisoned");
            return;
        };
        let session = match sessions.get(&session_id) {
            Some(s) => s,
            None => {
                error!("Agent {} session not found in stores", session_id);
                return;
            }
        };
        match agent_type {
            AgentKind::Implementer => session.work_id.clone(),
            AgentKind::Reviewer => session.bundle_id.clone(),
            // Already handled by is_thinking_plane() above
            AgentKind::Coordinator | AgentKind::Researcher | AgentKind::Integrator | AgentKind::Chat => None,
        }
    };

    if let Some(ref key) = worktree_key {
        let base_ref = resolve_worktree_base(&stores);
        info!("Agent {} using worktree base: {}", session_id, base_ref);
        let worktree_path = match worktree_mgr.get_or_create_branch(key, &base_ref) {
            Ok(path) => path,
            Err(e) => {
                error!("Agent {} worktree creation failed: {}", session_id, e);
                // Fail the session immediately - don't proceed without a worktree
                let Ok(mut sessions) = stores.write_agent_sessions() else {
                    error!("agent_sessions lock poisoned");
                    return;
                };
                if let Some(session) = sessions.get_mut(&session_id) {
                    let _ = session.transition_to(AgentStatus::Failed);
                    session.error_message = Some(format!("worktree creation failed: {}", e));
                    persist_session(&stores, session);
                }
                debug!("[agent_status] {}: -> Failed (worktree creation)", session_id);
                let _ = event_tx.send(DaemonEvent::agent_status_failed(
                    &session_id,
                    Some(format!("worktree creation failed: {}", e)),
                ));
                return;
            }
        };

        {
            let Ok(mut sessions) = stores.write_agent_sessions() else {
                error!("agent_sessions lock poisoned");
                return;
            };
            if let Some(session) = sessions.get_mut(&session_id) {
                session.worktree_path = Some(worktree_path.to_string_lossy().to_string());
                persist_session(&stores, session);
            }
            info!("Agent {} worktree created at {}", session_id, worktree_path.display());
        }
    }

    // Retain a copy of the worktree manager for cleanup after the agent loop exits.
    // The original is moved into the bridge.
    let cleanup_mgr = worktree_mgr.clone();
    let cleanup_key = worktree_key.clone();

    let prefix = format!("[{}:{}]", agent_type, session_id);
    info!("{} Agent task started", prefix);

    // Create the in-process IPC bridge for this agent
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

    // Transition to Running
    {
        let Ok(mut sessions) = stores.write_agent_sessions() else {
            error!("agent_sessions lock poisoned");
            return;
        };
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Err(e) = session.transition_to(AgentStatus::Running) {
                error!("{} failed to start: {}", prefix, e);
                return;
            }
            persist_session(&stores, session);
        }
    }
    debug!("[agent_status] {}: -> Running", session_id);
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Running));

    // Resolve session timeout from the agent's role config
    let timeout_secs = match agent_type {
        AgentKind::Implementer => stores.config.agents.implementer.session_timeout_secs,
        AgentKind::Reviewer => stores.config.agents.reviewer.session_timeout_secs,
        AgentKind::Researcher => stores.config.agents.researcher.session_timeout_secs,
        AgentKind::Coordinator => stores.config.agents.coordinator.role.session_timeout_secs,
        AgentKind::Integrator => stores.config.integrator.session_timeout_secs,
        AgentKind::Chat => None,
    };

    let loop_future = run_agent_loop(&session_id, agent_type, &stores, &bridge, &event_tx);

    let result = if let Some(secs) = timeout_secs {
        match tokio::time::timeout(Duration::from_secs(secs), loop_future).await {
            Ok(inner) => inner,
            Err(_) => {
                warn!("{} session timeout after {}s", prefix, secs);
                Err(eyre!("session timed out after {}s", secs))
            }
        }
    } else {
        loop_future.await
    };

    // Bundle-aware work handback (implementer only)
    if agent_type == AgentKind::Implementer
        && let Some(ref wi_id) = worktree_key
    {
        if let Some(target_status) = determine_work_handback(&stores, wi_id, &session_id, result.is_ok()) {
            let resp = bridge.request(
                "work.transition",
                serde_json::json!({ "id": wi_id, "target_status": target_status, "role": "implementer" }),
            );
            if resp.is_error() {
                warn!(
                    "{} failed to transition work {} -> {}: {:?}",
                    prefix, wi_id, target_status, resp.error
                );
            } else {
                info!("{} transitioned work {} -> {}", prefix, wi_id, target_status);
            }
        } else {
            info!(
                "{} skipping work handback for {} - sibling implementer still active",
                prefix, wi_id
            );
        }
    }

    // Release all advisory locks held by this agent's work ID
    if let Ok(sessions) = stores.read_agent_sessions()
        && let Some(session) = sessions.get(&session_id)
        && let Some(ref wi_id) = session.work_id
    {
        release_agent_locks(&bridge, wi_id, &prefix);
    }

    // Clean up worktree after agent loop exits (only for non-thinking-plane agents)
    if let Some(ref key) = cleanup_key {
        if let Err(e) = cleanup_mgr.cleanup(key) {
            warn!("{} worktree cleanup failed (key={}): {}", prefix, key, e);
        } else {
            info!("{} worktree cleaned up (key={})", prefix, key);
        }
    }

    // Transition to terminal state based on result
    let terminal_status = match result {
        Ok(_) => AgentStatus::Completed,
        Err(ref e) => {
            warn!("{} failed: {}", prefix, e);
            AgentStatus::Failed
        }
    };

    {
        let Ok(mut sessions) = stores.write_agent_sessions() else {
            error!("agent_sessions lock poisoned");
            return;
        };
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Err(e) = session.transition_to(terminal_status) {
                error!("{} failed to transition to {:?}: {}", prefix, terminal_status, e);
                return;
            }
            if let Err(ref e) = result {
                session.error_message = Some(e.to_string());
                session.error_kind = Some(crate::agents::error::classify_error(e));
            }
            persist_session(&stores, session);
        }
    }
    // Create a Learning from implementer failures so the Coordinator can adapt
    if terminal_status == AgentStatus::Failed && agent_type == AgentKind::Implementer {
        let (wi_title, wi_id, iteration_count) = {
            let Ok(sessions) = stores.read_agent_sessions() else {
                error!("agent_sessions lock poisoned");
                return;
            };
            let session = sessions.get(&session_id);
            let wi_id = session.and_then(|s| s.work_id.clone()).unwrap_or_default();
            let iteration = session.map(|s| s.iteration).unwrap_or(0);
            let title = if !wi_id.is_empty() {
                stores
                    .read_works()
                    .ok()
                    .and_then(|works| works.get(&wi_id).map(|w| w.title.clone()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (title, wi_id, iteration)
        };

        let error_msg = result.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
        let learning = crate::domain::learning::Learning::new(
            wi_id.clone(),
            crate::domain::learning::LearningScope::Phase,
            format!(
                "Implementer failed on '{}' after {} iterations. Error: {}. \
                 Consider: splitting into smaller tasks, providing more context, \
                 or checking dependency ordering.",
                wi_title, iteration_count, error_msg
            ),
        );
        let learning_id = learning.id.clone();
        if let Ok(mut learnings) = stores.write_learnings() {
            learnings.insert(learning_id.clone(), learning.clone());
        } else {
            error!("learnings lock poisoned");
        }
        if let Ok(Some(mut store_guard)) = stores.lock_store() {
            let _ = store_guard.create(learning);
        }
        info!("{} created failure learning {} on '{}'", prefix, learning_id, wi_title);
    }

    debug!("[agent_status] {}: -> {:?} (terminal)", session_id, terminal_status);
    if terminal_status == AgentStatus::Failed {
        let error = result.as_ref().err().map(|e| e.to_string());
        let _ = event_tx.send(DaemonEvent::agent_status_failed(&session_id, error));
    } else {
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, terminal_status));
    }

    info!("{} task finished -> {:?}", prefix, terminal_status);
}

/// Agent loop - dispatches to the appropriate agent implementation based on type.
pub(super) async fn run_agent_loop(
    session_id: &str,
    agent_type: AgentKind,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    debug!("[{}:{}] run_agent_loop()", agent_type, session_id);
    // Verify the bridge works by checking system status
    let status_resp = bridge.request("system.status", serde_json::json!(null));
    if status_resp.is_error() {
        return Err(eyre!("bridge health check failed: {:?}", status_resp.error));
    }
    info!("[{}:{}] Bridge health check passed", agent_type, session_id);

    let mut ctx = AgentContext::from_session_id(session_id, stores.clone(), event_tx.clone())?;

    // Per-session tool resolution: 3-layer priority stack
    //   Layer 1: config tools (from loopr.yml)
    //   Layer 2: runtime tools (from tools.register IPC)
    //   Layer 3: detected tools (from marker files)
    if let Some(ref wt_path) = ctx.session.worktree_path {
        let worktree = std::path::Path::new(wt_path);
        let runtime_tools = stores.read_runtime_tools()?;
        let resolved = crate::tools::resolve_tools(&stores.config.agents.tools, &runtime_tools, Some(worktree));
        drop(runtime_tools);
        ctx.tool_runner = Arc::new(crate::tools::ToolRunner::new(&resolved));
        ctx.tool_executor = Arc::new(crate::tools::ToolExecutor::standard(&resolved));
        info!(
            "[{}:{}] Session tool executor: {} tools (resolved from {})",
            agent_type,
            session_id,
            ctx.tool_executor.available_tools().len(),
            wt_path
        );
    }

    let (result, iteration) = match agent_type {
        AgentKind::Implementer => {
            let config = stores.config.agents.implementer.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;
            let work_id = ctx
                .session
                .work_id
                .clone()
                .ok_or_else(|| eyre!("implementer session missing work_id"))?;
            let worktree_path = ctx
                .session
                .worktree_path
                .as_ref()
                .map(std::path::PathBuf::from)
                .ok_or_else(|| eyre!("implementer session missing worktree_path"))?;
            let mut agent = implementer::ImplementerAgent::new(ctx, llm, config, work_id, worktree_path);
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentKind::Reviewer => {
            let config = stores.config.agents.reviewer.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;
            let mut agent = reviewer::ReviewerAgent::new(ctx, llm, config)?;
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentKind::Coordinator => {
            let config = stores.config.agents.coordinator.clone();
            let llm = create_llm_client(&config.role, session_id, event_tx)?;
            let mut agent = coordinator::CoordinatorAgent::new(ctx, llm, config);
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentKind::Researcher => {
            let config = stores.config.agents.researcher.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;
            let mut agent = researcher::ResearcherAgent::new(ctx, llm, config);
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentKind::Integrator => {
            let config = stores.config.integrator.clone();
            let mut agent = integrator::IntegratorAgent::new(ctx, config);
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentKind::Chat => {
            // Chat sessions are not started via run_agent_task - they use chat.submit IPC.
            // This arm should never be reached.
            return Ok(());
        }
    };

    // Unified iteration writeback
    {
        let mut sessions = stores.write_agent_sessions()?;
        if let Some(s) = sessions.get_mut(session_id) {
            s.iteration = iteration;
        }
    }

    result
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {

    use crate::agents::AgentKind;
    use crate::agents::executor::tests::test_stores;
    use crate::config::{Config, ProjectConfig};

    use crate::test_util::TestDir;

    use crate::agents::executor::run_agent_task;
    use crate::agents::{AgentSession, AgentStatus};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use taskstore::Store;
    use tokio::sync::broadcast;

    use crate::daemon::context::Stores;
    use crate::worktree::manager::WorktreeManager;

    use eyre::eyre;

    fn resolve_timeout(config: &Config, agent_type: AgentKind) -> Option<u64> {
        match agent_type {
            AgentKind::Implementer => config.agents.implementer.session_timeout_secs,
            AgentKind::Reviewer => config.agents.reviewer.session_timeout_secs,
            AgentKind::Researcher => config.agents.researcher.session_timeout_secs,
            AgentKind::Coordinator => config.agents.coordinator.role.session_timeout_secs,
            AgentKind::Integrator => config.integrator.session_timeout_secs,
            AgentKind::Chat => None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_agent_task_lifecycle() {
        let dir = TestDir::new("loopr-agent-task");

        let stores = test_stores(&dir);
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let session = AgentSession::new(AgentKind::Implementer, "test-model".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        run_agent_task(
            session_id.clone(),
            AgentKind::Implementer,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status().is_terminal());

        let mut events = vec![];
        while let Ok(e) = event_rx.try_recv() {
            events.push(e);
        }
        assert!(
            events.iter().any(|e| e.event == "agent.status_changed"),
            "expected agent status change events"
        );
    }

    // --- Group A: Record creation actions ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_agent_task_coordinator_restart_loop() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-exec-coordrestart");

        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            agents: crate::config::AgentConfig {
                coordinator: crate::config::CoordinatorConfig {
                    idle_interval_secs: 0,
                    active_interval_secs: 0,
                    ..crate::config::CoordinatorConfig::default()
                },
                ..crate::config::AgentConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(&dir).unwrap();
        let mut custom_stores = Stores::new();
        custom_stores.store = Some(Arc::new(StdMutex::new(store)));
        custom_stores.config = config;
        let stores = Arc::new(custom_stores);

        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let session = AgentSession::new(AgentKind::Coordinator, "test-model".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let stores_clone = stores.clone();
        let sid_clone = session_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut sessions = stores_clone.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&sid_clone) {
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        });

        run_agent_task(
            session_id.clone(),
            AgentKind::Coordinator,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(
            session.status().is_terminal(),
            "coordinator should be in terminal state"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_agent_task_researcher_flow() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-exec-resflow");

        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let mut session = AgentSession::new(AgentKind::Researcher, "test-model".to_string());
        session.target_id = Some("plan-1".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let stores_clone = stores.clone();
        let sid_clone = session_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut sessions = stores_clone.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&sid_clone) {
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        });

        run_agent_task(
            session_id.clone(),
            AgentKind::Researcher,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status().is_terminal(), "researcher should reach terminal state");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_agent_task_integrator_flow() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-exec-integflow");

        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            integrator: crate::config::IntegratorConfig {
                interval_secs: 0,
                enabled: true,
                ..crate::config::IntegratorConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(&dir).unwrap();
        let mut custom_stores = Stores::new();
        custom_stores.store = Some(Arc::new(StdMutex::new(store)));
        custom_stores.config = config;
        let stores = Arc::new(custom_stores);

        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let session = AgentSession::new(AgentKind::Integrator, "test-model".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let stores_clone = stores.clone();
        let sid_clone = session_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut sessions = stores_clone.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&sid_clone) {
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        });

        run_agent_task(
            session_id.clone(),
            AgentKind::Integrator,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status().is_terminal(), "integrator should reach terminal state");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_agent_task_worktree_cleanup() {
        let dir = TestDir::new("loopr-exec-wtcleanup");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".to_string());
        session.work_id = Some("wi-test-123".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        run_agent_task(
            session_id.clone(),
            AgentKind::Implementer,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status().is_terminal());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_session_timeout_defaults_per_agent_type() {
        let config = Config::default();

        assert_eq!(resolve_timeout(&config, AgentKind::Implementer), Some(1800));
        assert_eq!(resolve_timeout(&config, AgentKind::Reviewer), Some(600));
        assert_eq!(resolve_timeout(&config, AgentKind::Researcher), Some(600));
        assert_eq!(resolve_timeout(&config, AgentKind::Integrator), Some(1200));
        assert_eq!(resolve_timeout(&config, AgentKind::Coordinator), None);
        assert_eq!(resolve_timeout(&config, AgentKind::Chat), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_session_timeout_terminates_slow_future() {
        let slow_future = async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<(), eyre::Report>(())
        };

        let result = match tokio::time::timeout(Duration::from_millis(50), slow_future).await {
            Ok(inner) => inner,
            Err(_) => Err(eyre!("session timed out")),
        };

        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("timed out"), "expected timeout error, got: {}", msg);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_session_timeout_none_allows_completion() {
        let fast_future = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok::<(), eyre::Report>(())
        };

        let timeout_secs: Option<u64> = None;

        let result = if let Some(secs) = timeout_secs {
            match tokio::time::timeout(Duration::from_secs(secs), fast_future).await {
                Ok(inner) => inner,
                Err(_) => Err(eyre!("session timed out")),
            }
        } else {
            fast_future.await
        };

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_session_timeout_fast_future_completes_before_deadline() {
        let fast_future = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok::<(), eyre::Report>(())
        };

        let result = match tokio::time::timeout(Duration::from_secs(5), fast_future).await {
            Ok(inner) => inner,
            Err(_) => Err(eyre!("session timed out")),
        };

        assert!(result.is_ok(), "fast future should complete before timeout");
    }
}
