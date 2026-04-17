use std::sync::Arc;

use tokio::sync::broadcast;

use crate::config::Config;
use crate::daemon::context::Stores;
use crate::daemon::handlers;
use crate::fsm::runtime::FsmInterpreter;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse};
use crate::worktree::manager::WorktreeManager;

/// In-process channel for agent ↔ daemon communication.
/// Avoids the overhead of Unix socket for same-process agents.
/// Uses the same dispatch() function as socket-based IPC — same FSM
/// validation, role guards, and parent checks apply.
pub struct AgentIpcBridge {
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    worktree_mgr: WorktreeManager,
    config: Config,
    fsm: Arc<FsmInterpreter>,
    next_id: std::sync::atomic::AtomicU64,
}

impl AgentIpcBridge {
    pub fn new(
        stores: Arc<Stores>,
        event_tx: broadcast::Sender<DaemonEvent>,
        worktree_mgr: WorktreeManager,
        config: Config,
        fsm: Arc<FsmInterpreter>,
    ) -> Self {
        Self {
            stores,
            event_tx,
            worktree_mgr,
            config,
            fsm,
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Access the underlying stores.
    pub fn stores(&self) -> &Arc<Stores> {
        &self.stores
    }

    /// Access the configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Access the event broadcast channel.
    pub fn event_tx(&self) -> &broadcast::Sender<DaemonEvent> {
        &self.event_tx
    }

    /// Send a request through the handler pipeline, same as socket-based IPC.
    /// Uses block_in_place to bridge the sync→async gap: dispatch is async but
    /// all agent callers are sync. A future migration will make this method async
    /// and propagate .await through the agent call chain.
    pub fn request(&self, method: &str, params: serde_json::Value) -> DaemonResponse {
        tracing::debug!("AgentIpcBridge::request(method={})", method);
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let req = DaemonRequest::new(id, method, params);
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(handlers::dispatch(
                &self.stores,
                &self.event_tx,
                &self.worktree_mgr,
                &self.config.integrator,
                &self.fsm,
                req,
            ))
        })
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProjectConfig};
    use crate::test_util::TestDir;
    use taskstore::Store;

    fn test_bridge() -> (TestDir, AgentIpcBridge) {
        let dir = TestDir::new("loopr-bridge-test");
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let (event_tx, _) = broadcast::channel(16);
        let fsm = Arc::new(FsmInterpreter::embedded().unwrap());

        // Open TaskStore and rebuild indexes like DaemonContext does
        let store = Store::open(&dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));

        (
            dir,
            AgentIpcBridge::new(Arc::new(stores), event_tx, worktree_mgr, config, fsm),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bridge_handshake() {
        let (_dir, bridge) = test_bridge();
        let resp = bridge.request("system.handshake", serde_json::json!({"version": "0.1.0"}));
        assert!(!resp.is_error());
        assert!(resp.result.unwrap()["protocol"].is_string());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bridge_unknown_method() {
        let (_dir, bridge) = test_bridge();
        let resp = bridge.request("nonexistent.method", serde_json::json!(null));
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("nonexistent.method"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bridge_plan_create() {
        let (_dir, bridge) = test_bridge();
        let resp = bridge.request(
            "plan.create",
            serde_json::json!({
                "title": "Bridge Test Plan",
                "description": "Test description",
                "acceptance-criteria": "Test criteria"
            }),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Bridge Test Plan");
        assert!(result["id"].is_string());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bridge_increments_request_ids() {
        let (_dir, bridge) = test_bridge();
        let resp1 = bridge.request("system.handshake", serde_json::json!({"version": "0.1.0"}));
        let resp2 = bridge.request("system.handshake", serde_json::json!({"version": "0.1.0"}));
        assert_ne!(resp1.id, resp2.id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bridge_event_tx() {
        let (_dir, bridge) = test_bridge();
        let mut rx = bridge.event_tx().subscribe();
        let event = DaemonEvent::record_created("test", "t1");
        bridge.event_tx().send(event).unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received.event, "record.created");
    }
}
