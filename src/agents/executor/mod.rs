mod action;
pub(crate) mod context;
mod lifecycle;
mod llm;
pub mod result;
mod util;

// Re-exports: preserve the public API from the old single-file executor.rs
pub use action::execute_action;
pub use lifecycle::run_agent_task;
pub use result::ActionResult;
pub use util::resolve_worktree_base;

use std::sync::Arc;

use eyre::{Result, eyre};
use log::{debug, info, warn};
use tokio::sync::broadcast;

use crate::agents::{AgentSession, AgentStatus, AgentType};
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

/// Run a single Work item through the full Implementer lifecycle.
///
/// This is the entry point for pull-based workers. It:
/// 1. Transitions Work Ready -> InProgress (returns Ok if already claimed)
/// 2. Creates an AgentSession
/// 3. Delegates to `run_agent_task` for the full lifecycle (worktree, LLM, agent loop, handback, cleanup)
///
/// Race handling: if the Ready -> InProgress transition fails (another worker grabbed it),
/// returns Ok(()) immediately - this is expected contention, not an error.
pub async fn run_single_work(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    implementer_config: &crate::config::AgentRoleConfig,
    work_id: &str,
    worker_id: u32,
) -> Result<()> {
    info!("Worker {} attempting Work {}", worker_id, work_id);

    // Step 1: Transition Work Ready -> InProgress.
    // Use the bridge to go through the handler (FSM validation + persistence).
    let bridge = crate::agents::bridge::AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr.clone(),
        stores.config.clone(),
    );
    let transition_resp = bridge.request(
        "work.transition",
        serde_json::json!({
            "id": work_id,
            "target_status": "InProgress",
            "role": "coordinator",
            "assignee": "implementer",
        }),
    );
    if transition_resp.is_error() {
        // Expected contention: another worker grabbed this Work first.
        info!(
            "Worker {} could not claim Work {} (likely already claimed): {:?}",
            worker_id,
            work_id,
            transition_resp.error.as_ref().map(|e| &e.message)
        );
        return Ok(());
    }

    // Step 2: Check pool capacity and dedup before creating session
    {
        let sessions = stores.read_agent_sessions()?;
        let active_count = sessions
            .values()
            .filter(|s| s.agent_type == AgentType::Implementer && !s.status.is_terminal())
            .count();
        let max_pool = stores.config.agents.implementer.max_pool as usize;
        if active_count >= max_pool {
            warn!(
                "Worker {} pool exhausted ({}/{}), skipping Work {}",
                worker_id, active_count, max_pool, work_id
            );
            return Ok(());
        }

        // Dedup: if an implementer is already running on this work, skip
        let has_existing = sessions.values().any(|s| {
            s.agent_type == AgentType::Implementer && !s.status.is_terminal() && s.work_id.as_deref() == Some(work_id)
        });
        if has_existing {
            info!(
                "Worker {} skipping Work {} - implementer already running",
                worker_id, work_id
            );
            return Ok(());
        }
    }

    // Step 3: Create AgentSession
    let mut session = AgentSession::new(AgentType::Implementer, implementer_config.model.clone());
    session.work_id = Some(work_id.to_string());
    let session_id = session.id.clone();

    // Persist session
    if let Some(store) = &stores.store
        && let Ok(mut s) = store.lock().map_err(|_| eyre!("store lock poisoned"))
        && let Err(e) = s.create(session.clone())
    {
        warn!("Worker {} failed to persist session: {}", worker_id, e);
        return Err(eyre!("failed to create agent session: {}", e));
    }
    stores.write_agent_sessions()?.insert(session_id.clone(), session);
    let _ = event_tx.send(DaemonEvent::record_created("agent_session", &session_id));
    debug!("[agent_status] {}: -> Starting (worker spawn)", session_id);
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Starting));

    info!(
        "Worker {} created session {} for Work {}",
        worker_id, session_id, work_id
    );

    // Step 4: Run the full agent lifecycle (worktree, LLM, loop, handback, cleanup)
    run_agent_task(
        session_id,
        AgentType::Implementer,
        stores.clone(),
        event_tx.clone(),
        worktree_mgr.clone(),
    )
    .await;

    Ok(())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::agent_logger::AgentLogger;
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::{AgentContext, AgentType};
    use crate::config::{Config, ProjectConfig, ToolEntry};
    use crate::test_util::TestDir;
    use crate::tools::ToolRunner;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use taskstore::Store;
    use tokio::sync::broadcast;

    // Re-import types used in tests from submodules
    use crate::agents::AgentAction;
    use crate::domain::bundle::BundleStatus;
    use crate::domain::tick::TickStatus;
    use util::{determine_work_handback, release_agent_locks, resolve_latest_published_tick_id};

    fn test_agent_logger(dir: &Path) -> AgentLogger {
        use crate::agents::AgentType;
        let file_path = dir.join("test-executor.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        AgentLogger::_new_for_test(AgentType::Coordinator, "test-session", file, file_path)
    }

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

    /// Build a minimal AgentContext for executor tests.
    fn test_agent_context(
        dir: &Path,
        stores: &Arc<Stores>,
        agent_type: AgentType,
    ) -> (AgentContext, broadcast::Receiver<DaemonEvent>) {
        let (event_tx, event_rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let session = AgentSession::new(agent_type, "test-model".into());
        let ctx = AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.read_tool_runner().unwrap(),
            tool_executor: stores.read_tool_executor().unwrap(),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        };
        (ctx, event_rx)
    }

    /// Build a minimal AgentContext with custom ToolRunner entries.
    fn test_agent_context_with_tools(
        dir: &Path,
        stores: &Arc<Stores>,
        agent_type: AgentType,
        tool_entries: &[ToolEntry],
    ) -> AgentContext {
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let session = AgentSession::new(agent_type, "test-model".into());
        AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: Arc::new(ToolRunner::new(tool_entries)),
            tool_executor: Arc::new(crate::tools::ToolExecutor::standard(tool_entries)),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        }
    }

    /// Build a minimal AgentContext with a custom Config (e.g., for LockStrict tests).
    fn test_agent_context_with_config(
        dir: &Path,
        stores: &Arc<Stores>,
        agent_type: AgentType,
        config: Config,
    ) -> AgentContext {
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, config);
        let agent_log = test_agent_logger(dir);
        let session = AgentSession::new(agent_type, "test-model".into());
        AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.read_tool_runner().unwrap(),
            tool_executor: stores.read_tool_executor().unwrap(),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        }
    }

    /// Helper: create a full Plan->Spec->Phase->Work hierarchy in stores via bridge.
    fn create_test_hierarchy(bridge: &AgentIpcBridge) -> (String, String, String, String) {
        let plan_resp = bridge.request(
            "plan.create",
            serde_json::json!({"title": "Test Plan", "description": "desc", "acceptance_criteria": "pass"}),
        );
        let plan_id = plan_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        bridge.request(
            "plan.transition",
            serde_json::json!({"id": plan_id, "target_status": "active", "role": "coordinator", "skip_validation": true}),
        );
        let spec_resp = bridge.request(
            "spec.create",
            serde_json::json!({"plan_id": plan_id, "title": "Test Spec", "description": "desc"}),
        );
        let spec_id = spec_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        bridge.request(
            "spec.transition",
            serde_json::json!({"id": spec_id, "target_status": "active", "role": "coordinator", "skip_validation": true}),
        );
        let phase_resp = bridge.request(
            "phase.create",
            serde_json::json!({"spec_id": spec_id, "title": "Test Phase", "description": "desc", "order": 1}),
        );
        let phase_id = phase_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        bridge.request(
            "phase.transition",
            serde_json::json!({"id": phase_id, "target_status": "active", "role": "coordinator", "skip_validation": true}),
        );
        let wi_resp = bridge.request(
            "work.create",
            serde_json::json!({"phase_id": phase_id, "title": "Test WI", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id, phase_id, wi_id)
    }

    #[tokio::test]
    async fn test_transition_action_uses_correct_param() {
        let dir = TestDir::new("loopr-exec-transparam");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: wi_id.clone(),
            target_status: "Abandoned".to_string(),
            role: Some("coordinator".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "expected Transitioned, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_assign_agent_auto_transitions_draft() {
        let dir = TestDir::new("loopr-exec-autotrans");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        let wi_resp = ctx.bridge.request("work.get", serde_json::json!({"id": wi_id}));
        let wi_status = wi_resp
            .result
            .as_ref()
            .unwrap()
            .get("status")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(
            wi_status, "InProgress",
            "work item should be InProgress after auto-transition"
        );

        assert!(
            matches!(result, ActionResult::AgentSpawned { .. } | ActionResult::ActionError(_)),
            "expected AgentSpawned or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_action_run_tool() {
        let dir = TestDir::new("loopr-exec-test");

        let entries = vec![ToolEntry {
            name: "echo-test".to_string(),
            command: "echo hello".to_string(),
            timeout_secs: 10,
            worktree: true,
        }];
        let stores = test_stores(&dir);
        let ctx = test_agent_context_with_tools(&dir, &stores, AgentType::Implementer, &entries);

        let action = AgentAction::RunTool {
            tool: "echo-test".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::ToolRun(tool_result) = result {
            assert_eq!(tool_result.exit_code, 0);
            assert_eq!(tool_result.stdout.trim(), "hello");
        } else {
            panic!("expected ToolRun result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_write_file() {
        let dir = TestDir::new("loopr-exec-write");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "hello world".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));

        let content = std::fs::read_to_string(dir.join("test.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_write_file_lock_strict_blocks_other_agent() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-lockstrict");

        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentType::Implementer, config);

        let lock_resp = ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );
        assert!(!lock_resp.is_error());

        let action = AgentAction::WriteFile {
            path: "src/main.rs".to_string(),
            content: "should be blocked".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, Some("agent-2")).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("locked")),
            "expected ActionError for locked file, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_lock_strict_allows_holder_rewrite() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-lockholderrewrite");

        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentType::Implementer, config);

        let lock_resp = ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "wi-abc", "granted_by": "wi-abc" }),
        );
        assert!(!lock_resp.is_error());

        let action = AgentAction::WriteFile {
            path: "src/main.rs".to_string(),
            content: "holder can rewrite".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, Some("wi-abc")).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileWritten(_)),
            "expected FileWritten (holder should not self-block), got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_write_file_lock_advisory_allows() {
        let dir = TestDir::new("loopr-exec-lockadvisory");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let lock_resp = ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );
        assert!(!lock_resp.is_error());

        let action = AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "should work".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));
    }

    #[tokio::test]
    async fn test_execute_action_read_file() {
        let dir = TestDir::new("loopr-exec-read");
        std::fs::write(dir.join("read-me.txt"), "file content").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::ReadFile {
            path: "read-me.txt".to_string(),
            offset: None,
            limit: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = result {
            assert!(content.contains("file content"));
        } else {
            panic!("expected FileRead result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_done() {
        let dir = TestDir::new("loopr-exec-done");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::Done {
            summary: "All done".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::Done(summary) = result {
            assert_eq!(summary, "All done");
        } else {
            panic!("expected Done result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_need_help() {
        let dir = TestDir::new("loopr-exec-help");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::NeedHelp {
            reason: "Ambiguous spec".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::NeedHelp(reason) = result {
            assert_eq!(reason, "Ambiguous spec");
        } else {
            panic!("expected NeedHelp result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_unknown_tool() {
        let dir = TestDir::new("loopr-exec-unk");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::RunTool {
            tool: "nonexistent".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"), "Expected 'not found' in: {err}");
        assert!(err.contains("register_tool"), "Expected 'register_tool' hint in: {err}");
    }

    #[tokio::test]
    async fn test_execute_register_tool() {
        let dir = TestDir::new("loopr-exec-regtool");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Researcher);

        let action = AgentAction::RegisterTool {
            name: "my-echo".to_string(),
            command: "echo hello".to_string(),
            timeout_secs: 300,
            worktree: true,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ToolRegistered(ref n) if n == "my-echo"),
            "Expected ToolRegistered, got: {:?}",
            result
        );

        let rt = stores.read_runtime_tools().unwrap();
        assert!(rt.contains_key("my-echo"));
        assert_eq!(rt["my-echo"].command, "echo hello");
    }

    #[tokio::test]
    async fn test_execute_action_acquire_lock() {
        let dir = TestDir::new("loopr-exec-acqlock");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-123".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::LockAcquired(lock_id) = &result {
            assert!(!lock_id.is_empty());
            assert_ne!(lock_id, "unknown");
        } else {
            panic!("expected LockAcquired result, got {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_action_acquire_lock_conflict() {
        let dir = TestDir::new("loopr-exec-lockconf");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-100".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::LockAcquired(_)));

        let action2 = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-200".to_string(),
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        if let ActionResult::ActionError(msg) = &result2 {
            assert!(
                msg.contains("already locked"),
                "expected conflict message, got: {}",
                msg
            );
        } else {
            panic!("expected ActionError for lock conflict, got {:?}", result2);
        }
    }

    #[tokio::test]
    async fn test_execute_action_release_lock() {
        let dir = TestDir::new("loopr-exec-rellock");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let acquire_action = AgentAction::AcquireLock {
            resource: "src/lib.rs".to_string(),
            holder_id: "wi-456".to_string(),
        };
        let acquire_result = execute_action(&acquire_action, &ctx, &dir, None).await.unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = acquire_result {
            id
        } else {
            panic!("expected LockAcquired");
        };

        let release_action = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        let release_result = execute_action(&release_action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::LockReleased(id) = &release_result {
            assert_eq!(id, &lock_id);
        } else {
            panic!("expected LockReleased result, got {:?}", release_result);
        }
    }

    #[tokio::test]
    async fn test_execute_action_acquire_after_release() {
        let dir = TestDir::new("loopr-exec-reacq");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action1 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-1".to_string(),
        };
        let r1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = r1 {
            id
        } else {
            panic!("expected LockAcquired");
        };

        let release = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        execute_action(&release, &ctx, &dir, None).await.unwrap();

        let action2 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-2".to_string(),
        };
        let r2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(matches!(r2, ActionResult::LockAcquired(_)));
    }

    #[tokio::test]
    async fn test_run_agent_task_lifecycle() {
        let dir = TestDir::new("loopr-agent-task");

        let stores = test_stores(&dir);
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let session = AgentSession::new(AgentType::Implementer, "test-model".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        run_agent_task(
            session_id.clone(),
            AgentType::Implementer,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal());

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

    #[tokio::test]
    async fn test_execute_create_plan() {
        let dir = TestDir::new("loopr-exec-createplan");
        let stores = test_stores(&dir);

        let action = AgentAction::CreatePlan {
            title: "New Plan".to_string(),
            description: "Plan desc".to_string(),
            acceptance_criteria: "Tests pass".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "plans");
            assert!(!id.is_empty());
            assert_ne!(id, "unknown");
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_spec() {
        let dir = TestDir::new("loopr-exec-createspec");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, _, _, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::CreateSpec {
            plan_id,
            title: "New Spec".to_string(),
            description: "Spec desc".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "specs");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_phase() {
        let dir = TestDir::new("loopr-exec-createphase");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, spec_id, _, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::CreatePhase {
            spec_id,
            title: "New Phase".to_string(),
            description: "Phase desc".to_string(),
            order: 2,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "phases");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_work() {
        let dir = TestDir::new("loopr-exec-createwi");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, wi_id) = create_test_hierarchy(&ctx.bridge);
        ctx.bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id, "target_status": "Ready", "role": "coordinator"
            }),
        );

        let action = AgentAction::CreateWork {
            phase_id,
            title: "New WI".to_string(),
            description: "WI desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["tests pass".to_string()],
            dependencies: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "works");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    // --- Group B: Git operations ---

    #[tokio::test]
    async fn test_execute_commit_success() {
        let dir = TestDir::new("loopr-exec-commit");

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

        std::fs::write(dir.join("test.txt"), "hello").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::Commit {
            message: "test commit".to_string(),
            paths: vec![],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Committed(ref msg) if msg == "test commit"),
            "expected Committed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_commit_specific_paths() {
        let dir = TestDir::new("loopr-exec-commitpaths");

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

        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("b.txt"), "bbb").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::Commit {
            message: "add a.txt only".to_string(),
            paths: vec!["a.txt".to_string()],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::Committed(_)));
    }

    #[tokio::test]
    async fn test_execute_propose_bundle() {
        let dir = TestDir::new("loopr-exec-propbundle");

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
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec!["Implemented feature X".to_string()],
            noop_reason: None,
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref desc) if desc == "My bundle"),
            "expected BundleProposed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_propose_bundle_no_work() {
        let dir = TestDir::new("loopr-exec-propnowi");

        tokio::process::Command::new("git")
            .args(["init"])
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
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec![],
            noop_reason: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("work_id"));
    }

    // --- Group C: Agent management + domain actions ---

    #[tokio::test]
    async fn test_execute_create_learning_with_all_fields() {
        let dir = TestDir::new("loopr-exec-learning");
        let stores = test_stores(&dir);

        let action = AgentAction::CreateLearning {
            content: "Always add tests".to_string(),
            scope: "global".to_string(),
            source_id: "wi-1".to_string(),
            applicable_roles: Some(vec!["implementer".to_string()]),
            resource_tags: Some(vec!["src/".to_string()]),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::LearningCreated(ref c) if c == "Always add tests"),
            "expected LearningCreated, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_create_learning_minimal() {
        let dir = TestDir::new("loopr-exec-learnmin");
        let stores = test_stores(&dir);

        let action = AgentAction::CreateLearning {
            content: "Minimal learning".to_string(),
            scope: "work".to_string(),
            source_id: "wi-1".to_string(),
            applicable_roles: None,
            resource_tags: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::LearningCreated(_)));
    }

    #[tokio::test]
    async fn test_execute_spawn_researcher() {
        let dir = TestDir::new("loopr-exec-spawnres");
        let stores = test_stores(&dir);

        let action = AgentAction::SpawnResearcher {
            query: "How does auth work?".to_string(),
            scope_id: "plan-1".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::AgentSpawned { ref agent_type, .. } if agent_type == "researcher")
                || matches!(result, ActionResult::ActionError(_)),
            "expected AgentSpawned or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_validate_document() {
        let dir = TestDir::new("loopr-exec-valdoc");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, _, _, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ValidateDocument {
            collection: "plan".to_string(),
            id: plan_id,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(
                result,
                ActionResult::DocumentValidated { .. } | ActionResult::ActionError(_)
            ),
            "expected DocumentValidated or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_triage_bundle() {
        let dir = TestDir::new("loopr-exec-triage");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/test",
                "description": "Test bundle",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::TriageBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("Triaged")),
            "expected Transitioned to Triaged, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle() {
        let dir = TestDir::new("loopr-exec-accept");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accept",
                "description": "Accept test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("Accepted")),
            "expected Transitioned to Accepted, got: {:?}",
            result
        );
    }

    // --- Group D: Lifecycle paths ---

    #[tokio::test]
    async fn test_write_file_path_escape() {
        let dir = TestDir::new("loopr-exec-escape");
        let stores = test_stores(&dir);

        let action = AgentAction::WriteFile {
            path: "../../../etc/passwd".to_string(),
            content: "pwned".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let dir = TestDir::new("loopr-exec-readnf");
        let stores = test_stores(&dir);

        let action = AgentAction::ReadFile {
            path: "nonexistent.txt".to_string(),
            offset: None,
            limit: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_transition_role_inference() {
        let dir = TestDir::new("loopr-exec-roleinfer");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: wi_id,
            target_status: "Abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "expected Transitioned with inferred role, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_write_file_creates_parent_dirs() {
        let dir = TestDir::new("loopr-exec-writedirs");
        let stores = test_stores(&dir);

        let action = AgentAction::WriteFile {
            path: "deep/nested/dir/file.txt".to_string(),
            content: "nested content".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));
        let content = std::fs::read_to_string(dir.join("deep/nested/dir/file.txt")).unwrap();
        assert_eq!(content, "nested content");
    }

    #[tokio::test]
    async fn test_search_code_action() {
        let dir = TestDir::new("loopr-exec-searchcode");
        std::fs::write(dir.join("example.rs"), "fn main() { println!(\"hello\"); }").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::SearchCode {
            pattern: "fn main".to_string(),
            glob: None,
            path: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileRead(ref content) if content.contains("fn main"))
                || matches!(result, ActionResult::ActionError(_)),
            "got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_list_directory_action() {
        let dir = TestDir::new("loopr-exec-listdir");
        std::fs::write(dir.join("file1.txt"), "a").unwrap();
        std::fs::write(dir.join("file2.txt"), "b").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::ListDirectory { path: ".".to_string() };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileRead(ref content) if content.contains("file1.txt")),
            "got: {:?}",
            result
        );
    }

    // --- Task #6: Additional coverage tests ---

    #[tokio::test]
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

        let session = AgentSession::new(AgentType::Coordinator, "test-model".to_string());
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
            AgentType::Coordinator,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal(), "coordinator should be in terminal state");
    }

    #[tokio::test]
    async fn test_run_agent_task_researcher_flow() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-exec-resflow");

        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let mut session = AgentSession::new(AgentType::Researcher, "test-model".to_string());
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
            AgentType::Researcher,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal(), "researcher should reach terminal state");
    }

    #[tokio::test]
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

        let session = AgentSession::new(AgentType::Integrator, "test-model".to_string());
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
            AgentType::Integrator,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal(), "integrator should reach terminal state");
    }

    #[tokio::test]
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

        let mut session = AgentSession::new(AgentType::Implementer, "test-model".to_string());
        session.work_id = Some("wi-test-123".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        run_agent_task(
            session_id.clone(),
            AgentType::Implementer,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal());
    }

    #[tokio::test]
    async fn test_transition_role_inference_all_collections() {
        let dir = TestDir::new("loopr-exec-transall");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, spec_id, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::Transition {
            collection: "plans".to_string(),
            id: plan_id.clone(),
            target_status: "abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("plans")),
            "expected Transitioned for plans, got: {:?}",
            result
        );

        let action = AgentAction::Transition {
            collection: "specs".to_string(),
            id: spec_id.clone(),
            target_status: "abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::Transitioned(_)));

        let action = AgentAction::Transition {
            collection: "phases".to_string(),
            id: phase_id.clone(),
            target_status: "abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::Transitioned(_)));
    }

    #[tokio::test]
    async fn test_transition_assignee_validation_for_inprogress() {
        let dir = TestDir::new("loopr-exec-assignee");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: wi_id.clone(),
            target_status: "InProgress".to_string(),
            role: Some("coordinator".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err(), "InProgress without assignee should fail: {:?}", result);
    }

    #[tokio::test]
    async fn test_execute_create_plan_error_path() {
        let dir = TestDir::new("loopr-exec-plnerr");
        let stores = test_stores(&dir);

        let action1 = AgentAction::CreatePlan {
            title: "First Plan".to_string(),
            description: "desc".to_string(),
            acceptance_criteria: "pass".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result1, ActionResult::RecordCreated { .. }));

        let action2 = AgentAction::CreatePlan {
            title: "Second Plan".to_string(),
            description: "desc".to_string(),
            acceptance_criteria: "pass".to_string(),
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Draft Plan already exists")),
            "expected draft-awareness error, got: {:?}",
            result2
        );
    }

    #[tokio::test]
    async fn test_execute_create_spec_error_path() {
        let dir = TestDir::new("loopr-exec-specerr");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, _, _, _) = create_test_hierarchy(&ctx.bridge);

        let action1 = AgentAction::CreateSpec {
            plan_id: plan_id.clone(),
            title: "New Spec".to_string(),
            description: "desc".to_string(),
        };
        let result1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result1, ActionResult::RecordCreated { .. }));

        let action2 = AgentAction::CreateSpec {
            plan_id: plan_id.clone(),
            title: "Another Spec".to_string(),
            description: "desc".to_string(),
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Draft Spec already exists")),
            "expected draft-awareness error for spec, got: {:?}",
            result2
        );
    }

    #[tokio::test]
    async fn test_execute_create_phase_error_path() {
        let dir = TestDir::new("loopr-exec-phaseerr");
        let stores = test_stores(&dir);

        let action = AgentAction::CreatePhase {
            spec_id: "nonexistent-spec".to_string(),
            title: "New Phase".to_string(),
            description: "desc".to_string(),
            order: 1,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("create_phase failed")),
            "expected error for nonexistent spec, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_create_work_error_path() {
        let dir = TestDir::new("loopr-exec-wierr");
        let stores = test_stores(&dir);

        let action = AgentAction::CreateWork {
            phase_id: "nonexistent-phase".to_string(),
            title: "New WI".to_string(),
            description: "desc".to_string(),
            resource_tags: vec![],
            acceptance_criteria: vec![],
            dependencies: vec![],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("create_work failed")),
            "expected error for nonexistent phase, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_triage_bundle_full() {
        let dir = TestDir::new("loopr-exec-triagefull");
        let stores = test_stores(&dir);

        let action = AgentAction::TriageBundle {
            bundle_id: "bd-nonexistent".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("not found")),
            "expected not-found error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_triage_bundle_rejects_work_id() {
        let dir = TestDir::new("loopr-exec-triage-wkid");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::TriageBundle {
            bundle_id: "wk-12345".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("not a bundle ID")),
            "expected prefix validation error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle_rejects_work_id() {
        let dir = TestDir::new("loopr-exec-accept-wkid");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::AcceptBundle {
            bundle_id: "wk-12345".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("not a bundle ID")),
            "expected prefix validation error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle_full() {
        let dir = TestDir::new("loopr-exec-acceptfull");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accepterr",
                "description": "Accept err test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("Use triage_bundle first")),
            "expected corrective hint for Proposed bundle, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_triage_bundle_rejects_reviewed_bundle() {
        let dir = TestDir::new("loopr-exec-triage-rev");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/triage-rev",
                "description": "Triage reviewed test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        let action = AgentAction::TriageBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("Use accept_bundle instead")),
            "expected corrective hint for Reviewed bundle, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_accept_bundle_rejects_proposed_bundle() {
        let dir = TestDir::new("loopr-exec-accept-prop");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accept-prop",
                "description": "Accept proposed test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("Use triage_bundle first")),
            "expected corrective hint for Proposed bundle, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_spawn_researcher_via_action() {
        let dir = TestDir::new("loopr-exec-spawnres2");
        let stores = test_stores(&dir);

        let action = AgentAction::SpawnResearcher {
            query: "What patterns are used in the codebase?".to_string(),
            scope_id: "spec-1".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::AgentSpawned { ref agent_type, .. } if agent_type == "researcher")
                || matches!(result, ActionResult::ActionError(_)),
            "expected AgentSpawned or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_lock_conflict_policy_ignore() {
        let dir = TestDir::new("loopr-exec-lockign");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "locked.txt", "holder_id": "agent-other", "granted_by": "agent-other" }),
        );

        let action = AgentAction::WriteFile {
            path: "locked.txt".to_string(),
            content: "advisory allows this".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileWritten(_)),
            "expected write to succeed under advisory policy, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_lock_conflict_policy_warn() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-lockwarn");
        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentType::Coordinator, config);

        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "strict.txt", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );

        let action = AgentAction::WriteFile {
            path: "strict.txt".to_string(),
            content: "should be blocked".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("locked") && msg.contains("LockStrict")),
            "expected lock-blocked error, got: {:?}",
            result
        );
    }

    #[test]
    fn test_resolve_worktree_base_no_ticks() {
        let dir = TestDir::new("loopr-wt-base-none");
        let stores = test_stores(&dir);
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "HEAD");
    }

    #[test]
    fn test_resolve_worktree_base_no_published_ticks() {
        let dir = TestDir::new("loopr-wt-base-nopub");
        let stores = test_stores(&dir);
        {
            let mut ticks = stores.ticks.write().unwrap();
            let mut t = crate::domain::tick::Tick::new(1);
            t.integration_sha = Some("abc123".to_string());
            ticks.insert(t.id.clone(), t);
        }
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "HEAD");
    }

    #[test]
    fn test_resolve_worktree_base_picks_latest_published() {
        let dir = TestDir::new("loopr-wt-base-latest");
        let stores = test_stores(&dir);
        {
            let mut ticks = stores.ticks.write().unwrap();

            let mut t1 = crate::domain::tick::Tick::new(1);
            t1.status = crate::domain::tick::TickStatus::Published;
            t1.integration_sha = Some("sha_tick_1".to_string());
            ticks.insert(t1.id.clone(), t1);

            let mut t2 = crate::domain::tick::Tick::new(3);
            t2.status = crate::domain::tick::TickStatus::Published;
            t2.integration_sha = Some("sha_tick_3".to_string());
            ticks.insert(t2.id.clone(), t2);

            let mut t3 = crate::domain::tick::Tick::new(2);
            t3.status = crate::domain::tick::TickStatus::Published;
            t3.integration_sha = Some("sha_tick_2".to_string());
            ticks.insert(t3.id.clone(), t3);
        }
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "sha_tick_3");
    }

    #[test]
    fn test_resolve_worktree_base_published_without_sha_falls_back() {
        let dir = TestDir::new("loopr-wt-base-nosha");
        let stores = test_stores(&dir);
        {
            let mut ticks = stores.ticks.write().unwrap();
            let mut t = crate::domain::tick::Tick::new(1);
            t.status = crate::domain::tick::TickStatus::Published;
            t.integration_sha = None;
            ticks.insert(t.id.clone(), t);
        }
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "HEAD");
    }

    #[tokio::test]
    async fn test_assign_agent_dependency_not_met() {
        let dir = TestDir::new("loopr-exec-depnotmet");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let dep_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Dep WI",
                "description": "dep desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
            }),
        );
        let dep_id = dep_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let wi_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Work WI",
                "description": "work desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
                "dependencies": [dep_id],
            }),
        );
        let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::DependencyNotMet { .. }),
            "expected DependencyNotMet, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_assign_agent_dependency_met() {
        let dir = TestDir::new("loopr-exec-depmet");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let dep_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Dep WI Done",
                "description": "dep desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
            }),
        );
        let dep_id = dep_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        {
            let mut wis = stores.works.write().unwrap();
            if let Some(wi) = wis.get_mut(&dep_id) {
                wi.status = crate::domain::work::WorkStatus::Done;
                wi.updated_at = crate::id::now_millis();
                if let Some(store_arc) = &stores.store {
                    let _ = store_arc.lock().unwrap().update(wi.clone());
                }
            }
        }

        let wi_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Work WI With Met Dep",
                "description": "work desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
                "dependencies": [dep_id],
            }),
        );
        let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::AgentSpawned { .. }),
            "expected AgentSpawned, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_create_work_with_dependencies() {
        let dir = TestDir::new("loopr-exec-wideps");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Dependent WI".to_string(),
            description: "depends on first".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["tests pass".to_string()],
            dependencies: vec![wi_id.clone()],
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(matches!(result, ActionResult::RecordCreated { .. }));

        if let ActionResult::RecordCreated { id, .. } = result {
            let wi = stores.works.read().unwrap().get(&id).cloned().unwrap();
            assert_eq!(wi.dependencies, vec![wi_id]);
        }
    }

    #[tokio::test]
    async fn test_create_work_duplicate_rejected() {
        let dir = TestDir::new("loopr-exec-widup");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let action1 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Unique WI".to_string(),
            description: "desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let result1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result1, ActionResult::RecordCreated { .. }));

        let action2 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Unique WI".to_string(),
            description: "different desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Duplicate")),
            "expected duplicate error, got: {:?}",
            result2
        );
    }

    #[tokio::test]
    async fn test_create_work_duplicate_case_insensitive() {
        let dir = TestDir::new("loopr-exec-widupcase");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        let action1 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Add Login".to_string(),
            description: "desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let _ = execute_action(&action1, &ctx, &dir, None).await;

        let action2 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "add login".to_string(),
            description: "desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Duplicate")),
            "expected case-insensitive duplicate error, got: {:?}",
            result2
        );
    }

    // --- Fix #1: resolve_latest_published_tick_id tests ---

    #[test]
    fn test_resolve_latest_published_tick_id_none_at_bootstrap() {
        let dir = TestDir::new("loopr-exec-tickid-none");
        let stores = test_stores(&dir);
        let result = resolve_latest_published_tick_id(&stores);
        assert!(result.is_none(), "expected None at bootstrap, got: {:?}", result);
    }

    #[test]
    fn test_resolve_latest_published_tick_id_returns_published() {
        let dir = TestDir::new("loopr-exec-tickid-pub");
        let stores = test_stores(&dir);

        let tick = crate::domain::tick::Tick {
            id: "tick-1".to_string(),
            number: 1,
            status: TickStatus::Published,
            bundle_ids: vec![],
            attempted_bundle_ids: vec![],
            integration_sha: Some("abc123".to_string()),
            validation_log: String::new(),
            created_at: crate::id::now_millis(),
            updated_at: crate::id::now_millis(),
        };
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let result = resolve_latest_published_tick_id(&stores);
        assert_eq!(result, Some("tick-1".to_string()));
    }

    #[tokio::test]
    async fn test_propose_bundle_includes_base_tick_id() {
        let dir = TestDir::new("loopr-exec-propbase");

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
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let tick = crate::domain::tick::Tick {
            id: "tick-pub-1".to_string(),
            number: 1,
            status: TickStatus::Published,
            bundle_ids: vec![],
            attempted_bundle_ids: vec![],
            integration_sha: Some("abc123".to_string()),
            validation_log: String::new(),
            created_at: crate::id::now_millis(),
            updated_at: crate::id::now_millis(),
        };
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Bundle with base tick".to_string(),
            claims: vec!["claim".to_string()],
            noop_reason: None,
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Bundle with base tick"),
            "expected BundleProposed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_propose_bundle_uses_deterministic_branch_name() {
        let dir = TestDir::new("loopr-exec-f2branch");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);
        ctx.bridge.request(
            "work.transition",
            serde_json::json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator"}),
        );

        let _ = tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await;
        std::fs::write(dir.join("impl.txt"), "implementation").unwrap();
        let _ = tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await;

        let action = AgentAction::ProposeBundle {
            description: "Test bundle".to_string(),
            claims: vec!["implemented feature".to_string()],
            noop_reason: None,
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(_)),
            "expected BundleProposed, got: {:?}",
            result
        );
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.values().next().expect("should have one bundle");
        assert_eq!(
            bundle.branch_name,
            format!("agent/{}", wi_id),
            "bundle branch should be deterministic agent/<work_id>"
        );
    }

    // --- determine_work_handback tests ---

    #[test]
    fn test_handback_succeeded_no_bundles_returns_blocked() {
        let dir = TestDir::new("loopr-handback-ok");
        let stores = test_stores(&dir);
        let result = determine_work_handback(&stores, "wi-1", "sess-1", true);
        assert_eq!(result, Some("Blocked"));
    }

    #[test]
    fn test_handback_failed_no_bundles_returns_blocked() {
        let dir = TestDir::new("loopr-handback-nobd");
        let stores = test_stores(&dir);
        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("Blocked"));
    }

    #[test]
    fn test_handback_failed_with_active_bundle_returns_in_review() {
        let dir = TestDir::new("loopr-handback-actbd");
        let stores = test_stores(&dir);

        let mut bundle = crate::domain::bundle::Bundle::new(
            "wi-1".to_string(),
            None,
            "agent/wi-1".to_string(),
            vec!["claim".into()],
        );
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("InReview"));
    }

    #[test]
    fn test_handback_failed_all_rejected_bundles_returns_blocked() {
        let dir = TestDir::new("loopr-handback-rejbd");
        let stores = test_stores(&dir);

        let mut bundle = crate::domain::bundle::Bundle::new(
            "wi-1".to_string(),
            None,
            "agent/wi-1".to_string(),
            vec!["claim".into()],
        );
        bundle.status = BundleStatus::Rejected;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("Blocked"));
    }

    #[test]
    fn test_handback_failed_sibling_active_returns_none() {
        let dir = TestDir::new("loopr-handback-sib");
        let stores = test_stores(&dir);

        let mut sibling = AgentSession::new(AgentType::Implementer, "test-model".into());
        sibling.work_id = Some("wi-1".to_string());
        sibling.transition_to(AgentStatus::Running).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(sibling.id.clone(), sibling);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, None);
    }

    #[test]
    fn test_handback_failed_sibling_terminal_checks_bundles() {
        let dir = TestDir::new("loopr-handback-sibterm");
        let stores = test_stores(&dir);

        let mut sibling = AgentSession::new(AgentType::Implementer, "test-model".into());
        sibling.work_id = Some("wi-1".to_string());
        sibling.transition_to(AgentStatus::Running).unwrap();
        sibling.transition_to(AgentStatus::Completed).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(sibling.id.clone(), sibling);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("Blocked"));
    }

    // --- ReadFile dedup tests ---

    #[tokio::test]
    async fn test_read_file_dedup_returns_unchanged_on_second_read() {
        let dir = TestDir::new("loopr-exec-dedup");
        std::fs::write(dir.join("target.rs"), "line1\nline2\nline3\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };

        let r1 = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r1 {
            assert!(content.contains("line1"), "first read should return content");
        } else {
            panic!("expected FileRead, got: {:?}", r1);
        }

        let r2 = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r2 {
            assert!(
                content.contains("File unchanged since last read"),
                "second read should return dedup message, got: {}",
                content
            );
            assert!(content.contains("3"), "should mention total lines");
        } else {
            panic!("expected FileRead, got: {:?}", r2);
        }
    }

    #[tokio::test]
    async fn test_read_file_dedup_invalidated_by_write() {
        let dir = TestDir::new("loopr-exec-dedup-write");
        std::fs::write(dir.join("target.rs"), "original\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let read_action = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };

        execute_action(&read_action, &ctx, &dir, None).await.unwrap();

        let write_action = AgentAction::WriteFile {
            path: "target.rs".to_string(),
            content: "updated\n".to_string(),
        };
        execute_action(&write_action, &ctx, &dir, None).await.unwrap();

        let r3 = execute_action(&read_action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r3 {
            assert!(
                content.contains("updated"),
                "read after write should return fresh content, got: {}",
                content
            );
            assert!(!content.contains("File unchanged"), "should not be dedup after write");
        } else {
            panic!("expected FileRead, got: {:?}", r3);
        }
    }

    #[tokio::test]
    async fn test_read_file_dedup_invalidated_by_edit() {
        let dir = TestDir::new("loopr-exec-dedup-edit");
        std::fs::write(dir.join("target.rs"), "hello world\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let read_action = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };

        execute_action(&read_action, &ctx, &dir, None).await.unwrap();

        let edit_action = AgentAction::EditFile {
            path: "target.rs".to_string(),
            old_string: "hello".to_string(),
            new_string: "goodbye".to_string(),
        };
        execute_action(&edit_action, &ctx, &dir, None).await.unwrap();

        let r3 = execute_action(&read_action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r3 {
            assert!(
                content.contains("goodbye"),
                "read after edit should return fresh content, got: {}",
                content
            );
            assert!(!content.contains("File unchanged"), "should not be dedup after edit");
        } else {
            panic!("expected FileRead, got: {:?}", r3);
        }
    }

    #[tokio::test]
    async fn test_read_file_dedup_different_offset_no_dedup() {
        let dir = TestDir::new("loopr-exec-dedup-offset");
        std::fs::write(dir.join("target.rs"), "line1\nline2\nline3\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action1 = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };
        let action2 = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: Some(1),
            limit: None,
        };

        execute_action(&action1, &ctx, &dir, None).await.unwrap();

        let r2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r2 {
            assert!(
                !content.contains("File unchanged"),
                "different offset should not dedup, got: {}",
                content
            );
            assert!(content.contains("line1"));
        } else {
            panic!("expected FileRead, got: {:?}", r2);
        }
    }

    // --- Phase 1: Auto-Lock tests ---

    #[tokio::test]
    async fn test_write_file_auto_acquires_lock() {
        let dir = TestDir::new("loopr-exec-autolock-write");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::WriteFile {
            path: "src/lib.rs".to_string(),
            content: "hello".to_string(),
        };
        execute_action(&action, &ctx, &dir, Some("wi-100")).await.unwrap();

        let lock_resp = ctx.bridge.request(
            "lock.list",
            serde_json::json!({ "resource": "src/lib.rs", "holder_id": "wi-100", "active_only": true }),
        );
        let locks = lock_resp.result.as_ref().unwrap().as_array().unwrap();
        assert_eq!(locks.len(), 1, "expected 1 auto-acquired lock, got {}", locks.len());
        assert_eq!(locks[0]["holder_id"].as_str().unwrap(), "wi-100");
    }

    #[tokio::test]
    async fn test_edit_file_auto_acquires_lock() {
        let dir = TestDir::new("loopr-exec-autolock-edit");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "old content").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::EditFile {
            path: "src/lib.rs".to_string(),
            old_string: "old content".to_string(),
            new_string: "new content".to_string(),
        };
        execute_action(&action, &ctx, &dir, Some("wi-200")).await.unwrap();

        let lock_resp = ctx.bridge.request(
            "lock.list",
            serde_json::json!({ "resource": "src/lib.rs", "holder_id": "wi-200", "active_only": true }),
        );
        let locks = lock_resp.result.as_ref().unwrap().as_array().unwrap();
        assert_eq!(locks.len(), 1, "expected 1 auto-acquired lock");
    }

    #[tokio::test]
    async fn test_write_file_reuses_existing_lock() {
        let dir = TestDir::new("loopr-exec-autolock-reuse");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::WriteFile {
            path: "src/lib.rs".to_string(),
            content: "first".to_string(),
        };
        execute_action(&action, &ctx, &dir, Some("wi-300")).await.unwrap();
        let action2 = AgentAction::WriteFile {
            path: "src/lib.rs".to_string(),
            content: "second".to_string(),
        };
        execute_action(&action2, &ctx, &dir, Some("wi-300")).await.unwrap();

        let lock_resp = ctx.bridge.request(
            "lock.list",
            serde_json::json!({ "resource": "src/lib.rs", "holder_id": "wi-300", "active_only": true }),
        );
        let locks = lock_resp.result.as_ref().unwrap().as_array().unwrap();
        assert_eq!(locks.len(), 1, "expected 1 lock (reused), got {}", locks.len());
    }

    #[tokio::test]
    async fn test_no_auto_lock_without_work_id() {
        let dir = TestDir::new("loopr-exec-autolock-none");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::WriteFile {
            path: "src/lib.rs".to_string(),
            content: "hello".to_string(),
        };
        execute_action(&action, &ctx, &dir, None).await.unwrap();

        let lock_resp = ctx.bridge.request(
            "lock.list",
            serde_json::json!({ "resource": "src/lib.rs", "active_only": true }),
        );
        let locks = lock_resp.result.as_ref().unwrap().as_array().unwrap();
        assert!(locks.is_empty(), "expected no locks when work_id is None");
    }

    #[test]
    fn test_release_agent_locks_cleans_up() {
        let dir = TestDir::new("loopr-exec-rellock-cleanup");
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(&dir);

        bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/a.rs", "holder_id": "wi-rel", "granted_by": "wi-rel" }),
        );
        bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/b.rs", "holder_id": "wi-rel", "granted_by": "wi-rel" }),
        );

        let check = bridge.request(
            "lock.list",
            serde_json::json!({ "holder_id": "wi-rel", "active_only": true }),
        );
        assert_eq!(check.result.as_ref().unwrap().as_array().unwrap().len(), 2);

        release_agent_locks(&bridge, "wi-rel", &agent_log);

        let after = bridge.request(
            "lock.list",
            serde_json::json!({ "holder_id": "wi-rel", "active_only": true }),
        );
        let remaining = after.result.as_ref().unwrap().as_array().unwrap();
        assert!(
            remaining.is_empty(),
            "expected 0 active locks after release, got {}",
            remaining.len()
        );
    }

    #[tokio::test]
    async fn test_edit_file_lock_strict_allows_holder() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-editlock-holder");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "original").unwrap();

        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentType::Implementer, config);

        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "wi-edit", "granted_by": "wi-edit" }),
        );

        let action = AgentAction::EditFile {
            path: "src/main.rs".to_string(),
            old_string: "original".to_string(),
            new_string: "modified".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, Some("wi-edit")).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileEdited(_)),
            "expected FileEdited (holder should not self-block on edit), got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_edit_file_lock_strict_blocks_other_agent() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-editlock-other");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "original").unwrap();

        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentType::Implementer, config);

        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );

        let action = AgentAction::EditFile {
            path: "src/main.rs".to_string(),
            old_string: "original".to_string(),
            new_string: "modified".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, Some("agent-2")).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("locked")),
            "expected ActionError for locked file, got: {:?}",
            result
        );
    }

    // -- Phase 2: Session Timeout tests --

    fn resolve_timeout(config: &Config, agent_type: AgentType) -> Option<u64> {
        match agent_type {
            AgentType::Implementer => config.agents.implementer.session_timeout_secs,
            AgentType::Reviewer => config.agents.reviewer.session_timeout_secs,
            AgentType::Researcher => config.agents.researcher.session_timeout_secs,
            AgentType::Coordinator => config.agents.coordinator.role.session_timeout_secs,
            AgentType::Integrator => config.integrator.session_timeout_secs,
            AgentType::Chat => None,
        }
    }

    #[test]
    fn test_session_timeout_defaults_per_agent_type() {
        let config = Config::default();

        assert_eq!(resolve_timeout(&config, AgentType::Implementer), Some(1800));
        assert_eq!(resolve_timeout(&config, AgentType::Reviewer), Some(600));
        assert_eq!(resolve_timeout(&config, AgentType::Researcher), Some(600));
        assert_eq!(resolve_timeout(&config, AgentType::Integrator), Some(1200));
        assert_eq!(resolve_timeout(&config, AgentType::Coordinator), None);
        assert_eq!(resolve_timeout(&config, AgentType::Chat), None);
    }

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
    async fn test_assign_agent_to_done_work_returns_directive_error() {
        let dir = TestDir::new("loopr-exec-assigndone");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        {
            let mut works = stores.works.write().unwrap();
            if let Some(wi) = works.get_mut(&wi_id) {
                wi.status = crate::domain::work::WorkStatus::Done;
                let wi_clone = wi.clone();
                if let Some(store) = &stores.store {
                    let _ = store.lock().unwrap().update(wi_clone);
                }
            }
        }

        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        match result {
            ActionResult::ActionError(msg) => {
                assert!(msg.contains("INVALID"), "error should be directive: {}", msg);
                assert!(
                    msg.contains("MUST NOT assign"),
                    "error should instruct LLM not to assign: {}",
                    msg
                );
                assert!(
                    msg.contains("Ready tasks instead"),
                    "error should redirect to Ready tasks: {}",
                    msg
                );
            }
            other => panic!("expected ActionError, got: {:?}", other),
        }
    }

    // --- Merged Bundle Override Guard tests ---

    fn create_work_at_inreview_with_bundle(
        bridge: &AgentIpcBridge,
        stores: &Arc<Stores>,
        bundle_status: &str,
    ) -> (String, String) {
        let (_, _, _, wi_id) = create_test_hierarchy(bridge);

        bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "coordinator",
                "assignee": "test-impl",
            }),
        );

        let bundle_resp = bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/guard-test",
                "description": "guard test bundle",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id,
                "target_status": "InReview",
                "role": "implementer",
            }),
        );

        let chain: Vec<(&str, &str)> = match bundle_status {
            "Proposed" => vec![],
            "Triaged" => vec![("Triaged", "coordinator")],
            "Reviewed" => vec![("Triaged", "coordinator"), ("Reviewed", "reviewer")],
            "Accepted" => vec![
                ("Triaged", "coordinator"),
                ("Reviewed", "reviewer"),
                ("Accepted", "coordinator"),
            ],
            "Integrating" => vec![
                ("Triaged", "coordinator"),
                ("Reviewed", "reviewer"),
                ("Accepted", "coordinator"),
                ("Integrating", "integrator"),
            ],
            "Merged" => vec![
                ("Triaged", "coordinator"),
                ("Reviewed", "reviewer"),
                ("Accepted", "coordinator"),
                ("Integrating", "integrator"),
                ("Merged", "integrator"),
            ],
            "Rejected" => vec![("Triaged", "coordinator"), ("Rejected", "coordinator")],
            _ => vec![],
        };

        for (status, role) in chain {
            let mut params = serde_json::json!({
                "id": bundle_id,
                "target_status": status,
                "role": role,
            });
            if status == "Reviewed" {
                params["verification"] = serde_json::json!("tests passed");
            }
            bridge.request("bundle.transition", params);
        }

        {
            let mut bundles = stores.write_bundles().unwrap();
            if let Some(b) = bundles.get_mut(&bundle_id) {
                b.status = match bundle_status {
                    "Merged" => BundleStatus::Merged,
                    "Integrating" => BundleStatus::Integrating,
                    "Rejected" => BundleStatus::Rejected,
                    _ => b.status,
                };
            }
        }

        (wi_id, bundle_id)
    }

    #[tokio::test]
    async fn test_override_guard_merged_blocks_ready() {
        let dir = TestDir::new("loopr-exec-guard-merged");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (wi_id, bundle_id) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Merged");

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Ready".to_string(),
            reason: "stale rejection".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        match result {
            ActionResult::ActionError(msg) => {
                assert!(
                    msg.contains(&bundle_id),
                    "error should name the blocking bundle: {}",
                    msg
                );
                assert!(msg.contains("Merged"), "error should mention Merged status: {}", msg);
                assert!(
                    msg.contains("Do not retry"),
                    "error should tell LLM to back off: {}",
                    msg
                );
            }
            other => panic!("expected ActionError, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_override_guard_integrating_blocks_ready() {
        let dir = TestDir::new("loopr-exec-guard-integrating");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (wi_id, bundle_id) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Integrating");

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Ready".to_string(),
            reason: "stale rejection".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        match result {
            ActionResult::ActionError(msg) => {
                assert!(
                    msg.contains(&bundle_id),
                    "error should name the blocking bundle: {}",
                    msg
                );
                assert!(
                    msg.contains("Integrating"),
                    "error should mention Integrating status: {}",
                    msg
                );
            }
            other => panic!("expected ActionError, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_override_guard_rejected_allows_ready() {
        let dir = TestDir::new("loopr-exec-guard-rejected");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (wi_id, _) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Rejected");

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Ready".to_string(),
            reason: "no valid bundle".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "override to Ready should succeed with only Rejected bundles, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_override_guard_merged_allows_abandoned() {
        let dir = TestDir::new("loopr-exec-guard-abandoned");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (wi_id, _) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Merged");

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Abandoned".to_string(),
            reason: "pruning dead end".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "override to Abandoned should bypass guard even with Merged bundle, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_override_guard_mixed_bundles_merged_blocks() {
        let dir = TestDir::new("loopr-exec-guard-mixed");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (wi_id, _) = create_work_at_inreview_with_bundle(&ctx.bridge, &stores, "Merged");

        let bundle2_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/guard-test-2",
                "description": "second bundle",
            }),
        );
        let bundle2_id = bundle2_resp.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        {
            let mut bundles = stores.write_bundles().unwrap();
            if let Some(b) = bundles.get_mut(&bundle2_id) {
                b.status = BundleStatus::Rejected;
            }
        }

        let action = AgentAction::OverrideWork {
            work_id: wi_id.clone(),
            target_status: "Ready".to_string(),
            reason: "stale rejection".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::ActionError(_)),
            "Merged bundle should block even when a Rejected bundle also exists, got: {:?}",
            result
        );
    }

    // --- Noop Bundle Pathway tests ---

    #[tokio::test]
    async fn test_noop_propose_bundle_creates_empty_branch() {
        let dir = TestDir::new("loopr-exec-noop-branch");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Work already complete".to_string(),
            claims: vec!["criteria satisfied".to_string()],
            noop_reason: Some("Phase 1 already implemented this".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Work already complete"),
            "expected BundleProposed, got: {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles
            .values()
            .find(|b| b.work_id == wi_id)
            .expect("should have bundle");
        assert!(
            bundle.branch_name.is_empty(),
            "noop bundle should have empty branch_name"
        );
        assert_eq!(bundle.noop_reason.as_deref(), Some("Phase 1 already implemented this"));
    }

    #[tokio::test]
    async fn test_noop_bundle_handler_rejects_empty_branch_without_noop() {
        let dir = TestDir::new("loopr-exec-noop-reject");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "",
                "description": "should fail",
            }),
        );
        assert!(resp.is_error(), "empty branch_name without noop_reason should fail");
        let err_msg = resp.error.as_ref().unwrap().message.clone();
        assert!(
            err_msg.contains("branch_name"),
            "error should mention branch_name: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_noop_guard_rejects_dirty_worktree() {
        let dir = TestDir::new("loopr-exec-noop-dirty");

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

        std::fs::write(dir.join("todo.lua"), "print('hello')").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);
        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Work already complete".to_string(),
            claims: vec!["criteria satisfied".to_string()],
            noop_reason: Some("already done".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("uncommitted changes")),
            "expected ActionError for dirty worktree noop, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_noop_guard_allows_clean_worktree() {
        let dir = TestDir::new("loopr-exec-noop-clean");

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

        std::fs::write(dir.join(".gitignore"), ".taskstore/\n*.log\n").unwrap();
        tokio::process::Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "add gitignore"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);
        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Work already complete".to_string(),
            claims: vec!["criteria satisfied".to_string()],
            noop_reason: Some("Phase 1 already implemented this".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Work already complete"),
            "expected BundleProposed for clean noop, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_noop_bundle_handler_allows_empty_branch_with_noop() {
        let dir = TestDir::new("loopr-exec-noop-allow");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "",
                "noop_reason": "already done by phase 1",
                "description": "noop bundle",
            }),
        );
        assert!(
            !resp.is_error(),
            "empty branch_name with noop_reason should succeed: {:?}",
            resp.error
        );
        let bundle_id = resp.result.as_ref().unwrap()["id"].as_str().unwrap();
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(bundle_id).expect("bundle should exist");
        assert!(bundle.branch_name.is_empty());
        assert_eq!(bundle.noop_reason.as_deref(), Some("already done by phase 1"));
    }
}
