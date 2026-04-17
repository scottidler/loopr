#![allow(unused_imports, dead_code)]
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::{AgentContext, AgentKind, AgentSession};
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
    agent_type: AgentKind,
) -> (AgentContext, broadcast::Receiver<DaemonEvent>) {
    let (event_tx, event_rx) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );
    let session = AgentSession::new(agent_type, "test-model".into());
    let ctx = AgentContext {
        session,
        stores: stores.clone(),
        bridge,
        event_tx,
        event_rx: None,
        user_message_rx: None,
        tool_runner: stores.read_tool_runner().unwrap(),
        tool_executor: stores.read_tool_executor().unwrap(),
        read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
    };
    (ctx, event_rx)
}

/// Build a minimal AgentContext with custom ToolRunner entries.
pub(crate) fn test_agent_context_with_tools(
    dir: &Path,
    stores: &Arc<Stores>,
    agent_type: AgentKind,
    tool_entries: &[ToolEntry],
) -> AgentContext {
    let (event_tx, _) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );
    let session = AgentSession::new(agent_type, "test-model".into());
    AgentContext {
        session,
        stores: stores.clone(),
        bridge,
        event_tx,
        event_rx: None,
        user_message_rx: None,
        tool_runner: Arc::new(ToolRunner::new(tool_entries)),
        tool_executor: Arc::new(crate::tools::ToolExecutor::standard(tool_entries)),
        read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
    }
}

/// Build a minimal AgentContext with a custom Config (e.g., for LockStrict tests).
pub(crate) fn test_agent_context_with_config(
    dir: &Path,
    stores: &Arc<Stores>,
    agent_type: AgentKind,
    config: Config,
) -> AgentContext {
    let (event_tx, _) = broadcast::channel(16);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr,
        config,
        stores.fsm.clone(),
    );
    let session = AgentSession::new(agent_type, "test-model".into());
    AgentContext {
        session,
        stores: stores.clone(),
        bridge,
        event_tx,
        event_rx: None,
        user_message_rx: None,
        tool_runner: stores.read_tool_runner().unwrap(),
        tool_executor: stores.read_tool_executor().unwrap(),
        read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
    }
}

/// Helper: create a full Plan->Spec->Phase->Work hierarchy in stores via bridge.
pub(crate) fn create_test_hierarchy(bridge: &AgentIpcBridge) -> (String, String, String, String) {
    let plan_resp = bridge.request(
        "plan.create",
        serde_json::json!({"title": "Test Plan", "description": "desc", "acceptance-criteria": "pass"}),
    );
    let plan_id = plan_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
    bridge.request(
        "plan.transition",
        serde_json::json!({"id": plan_id, "target-status": "active", "role": "coordinator", "skip-validation": true}),
    );
    let spec_resp = bridge.request(
        "spec.create",
        serde_json::json!({"parent-id": plan_id, "title": "Test Spec", "description": "desc"}),
    );
    let spec_id = spec_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
    bridge.request(
        "spec.transition",
        serde_json::json!({"id": spec_id, "target-status": "active", "role": "coordinator", "skip-validation": true}),
    );
    let phase_resp = bridge.request(
        "phase.create",
        serde_json::json!({"parent-id": spec_id, "title": "Test Phase", "description": "desc", "order": 1}),
    );
    let phase_id = phase_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
    bridge.request(
        "phase.transition",
        serde_json::json!({"id": phase_id, "target-status": "active", "role": "coordinator", "skip-validation": true}),
    );
    let wi_resp = bridge.request(
        "work.create",
        serde_json::json!({"parent-id": phase_id, "title": "Test WI", "description": "desc", "files": ["src/"], "acceptance-criteria": ["tests pass"]}),
    );
    let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
    (plan_id, spec_id, phase_id, wi_id)
}
