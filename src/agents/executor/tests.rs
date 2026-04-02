use crate::agents::agent_logger::AgentLogger;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::{AgentContext, AgentSession, AgentType};
use crate::config::{Config, ProjectConfig, ToolEntry};
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::ToolRunner;
use crate::worktree::manager::WorktreeManager;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use taskstore::Store;
use tokio::sync::broadcast;

pub(crate) fn test_agent_logger(dir: &Path) -> AgentLogger {
    use crate::agents::AgentType;
    let file_path = dir.join("test-executor.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_path)
        .unwrap();
    AgentLogger::_new_for_test(AgentType::Coordinator, "test-session", file, file_path)
}

pub(crate) fn test_stores(dir: &Path) -> Arc<Stores> {
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
pub(crate) fn test_agent_context(
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
pub(crate) fn test_agent_context_with_tools(
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
pub(crate) fn test_agent_context_with_config(
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
pub(crate) fn create_test_hierarchy(bridge: &AgentIpcBridge) -> (String, String, String, String) {
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
