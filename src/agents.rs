pub mod action;
pub mod bridge;
pub mod cache;
pub mod context;
pub mod coordinator;
pub mod error;
pub mod event;
pub mod executor;
pub mod generation;
pub mod implementer;
pub mod integrator;
pub mod kind;
pub mod lifeguard;
pub mod llm_client;
pub mod researcher;
pub mod reviewer;
pub mod sandbox;
pub mod session;
pub mod status;
pub mod worker;

pub use self::action::AgentAction;
pub use self::event::AgentEvent;
pub use self::kind::AgentKind;
pub use self::session::AgentSession;
pub use self::status::AgentStatus;

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use eyre::Result;
use log::{debug, error, info, trace, warn};
use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::cache::ReadCache;
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::ToolExecutor;
use crate::tools::ToolRunner;
use crate::worktree::manager::WorktreeManager;

/// Common trait for all agent implementations.
pub trait Agent: Send {
    /// Run the agent's main loop to completion.
    fn run(&mut self) -> impl Future<Output = Result<()>> + Send;

    /// Agent type for dispatch and logging.
    fn agent_type(&self) -> AgentKind;
}

/// Shared cross-cutting fields for all agents.
pub struct AgentContext {
    pub session: AgentSession,
    pub stores: Arc<Stores>,
    pub bridge: AgentIpcBridge,
    pub event_tx: broadcast::Sender<DaemonEvent>,
    pub tool_runner: Arc<ToolRunner>,
    pub tool_executor: Arc<ToolExecutor>,
    pub read_cache: Mutex<ReadCache>,
}

impl AgentContext {
    /// Create an AgentContext by cloning the session from stores.
    pub fn from_session_id(
        session_id: &str,
        stores: Arc<Stores>,
        event_tx: broadcast::Sender<DaemonEvent>,
    ) -> eyre::Result<Self> {
        let session = {
            let sessions = stores.read_agent_sessions()?;
            sessions
                .get(session_id)
                .ok_or_else(|| eyre::eyre!("session not found: {}", session_id))?
                .clone()
        };

        let bridge = AgentIpcBridge::new(
            stores.clone(),
            event_tx.clone(),
            WorktreeManager::new(
                stores.config.project.repo_path.clone(),
                stores.config.project.repo_path.join(".worktrees"),
            ),
            stores.config.clone(),
        );

        Ok(Self {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.read_tool_runner()?,
            tool_executor: stores.read_tool_executor()?,
            read_cache: Mutex::new(ReadCache::default()),
        })
    }

    /// Acquire the read_cache lock, recovering from poison by logging a warning.
    /// Named `cache()` to avoid shadowing the `read_cache` field.
    pub fn cache(&self) -> MutexGuard<'_, ReadCache> {
        match self.read_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("read_cache lock was poisoned, recovering");
                poisoned.into_inner()
            }
        }
    }

    pub fn trace(&self, msg: &str) {
        trace!("[{}:{}] {}", self.session.agent_type, self.session.id, msg);
    }
    pub fn info(&self, msg: &str) {
        info!("[{}:{}] {}", self.session.agent_type, self.session.id, msg);
    }
    pub fn warn(&self, msg: &str) {
        warn!("[{}:{}] {}", self.session.agent_type, self.session.id, msg);
    }
    pub fn debug(&self, msg: &str) {
        debug!("[{}:{}] {}", self.session.agent_type, self.session.id, msg);
    }
    pub fn error(&self, msg: &str) {
        error!("[{}:{}] {}", self.session.agent_type, self.session.id, msg);
    }

    /// Check if this agent's session has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        let Ok(sessions) = self.stores.read_agent_sessions() else {
            return true;
        };
        sessions
            .get(&self.session.id)
            .map(|s| s.status() == AgentStatus::Cancelled)
            .unwrap_or(true)
    }

    /// Persist current iteration count to stores.
    pub fn persist_iteration(&self) {
        if let Ok(mut sessions) = self.stores.write_agent_sessions()
            && let Some(s) = sessions.get_mut(&self.session.id)
        {
            s.iteration = self.session.iteration;
        }
    }

    /// Emit an iteration-completed event.
    pub fn emit_iteration_completed(&self, iteration: u32, summary: &str) {
        let _ = self.event_tx.send(DaemonEvent::agent_iteration_completed(
            &self.session.id,
            iteration,
            summary,
        ));
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use taskstore::Store;
    use tokio::sync::broadcast;

    use super::*;
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::cache::ReadCache;
    use crate::test_util::TestDir;
    use crate::worktree::manager::WorktreeManager;

    // --- Test helpers ---

    fn make_test_dir(label: &str) -> TestDir {
        TestDir::new(&format!("loopr-mod-{label}"))
    }

    fn test_stores_with_dir(dir: &Path) -> Arc<crate::daemon::context::Stores> {
        use crate::config::{Config, ProjectConfig};
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(dir).unwrap();
        let mut stores = crate::daemon::context::Stores::new();
        stores.store = Some(Arc::new(StdMutex::new(store)));
        stores.config = config;
        Arc::new(stores)
    }

    fn test_agent_context(
        dir: &Path,
        stores: &Arc<crate::daemon::context::Stores>,
        agent_type: AgentKind,
    ) -> (AgentContext, broadcast::Receiver<DaemonEvent>) {
        let (event_tx, event_rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let session = AgentSession::new(agent_type, "test-model".into());
        let ctx = AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.read_tool_runner().unwrap(),
            tool_executor: stores.read_tool_executor().unwrap(),
            read_cache: Mutex::new(ReadCache::default()),
        };
        (ctx, event_rx)
    }

    // --- AgentContext tests ---

    #[test]
    fn test_agent_context_from_session_id_success() {
        let dir = make_test_dir("from-session-ok");
        let stores = test_stores_with_dir(&dir);

        // Insert a session into stores
        let session = AgentSession::new(AgentKind::Implementer, "test-model".into());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let (event_tx, _event_rx) = broadcast::channel(16);
        let ctx = AgentContext::from_session_id(&session_id, stores.clone(), event_tx);
        assert!(ctx.is_ok());
        let ctx = ctx.unwrap();
        assert_eq!(ctx.session.id, session_id);
        assert_eq!(ctx.session.agent_type, AgentKind::Implementer);
    }

    #[test]
    fn test_agent_context_from_session_id_not_found() {
        let dir = make_test_dir("from-session-missing");
        let stores = test_stores_with_dir(&dir);

        let (event_tx, _event_rx) = broadcast::channel(16);
        let result = AgentContext::from_session_id("nonexistent", stores, event_tx);
        let err_msg = result.err().expect("expected Err").to_string();
        assert!(
            err_msg.contains("session not found"),
            "expected 'session not found', got: {err_msg}"
        );
    }

    #[test]
    fn test_agent_context_log_delegates() {
        let dir = make_test_dir("log-delegates");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        // These should not panic - they call standard log macros with [component:id] prefix
        ctx.info("info message");
        ctx.warn("warn message");
        ctx.debug("debug message");
        ctx.error("error message");
    }

    #[test]
    fn test_agent_context_is_cancelled_false_when_running() {
        let dir = make_test_dir("cancel-false");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        // Insert session into stores with Starting status
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(ctx.session.id.clone(), ctx.session.clone());

        assert!(!ctx.is_cancelled());
    }

    #[test]
    fn test_agent_context_is_cancelled_true_when_cancelled() {
        let dir = make_test_dir("cancel-true");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        // Insert session with Cancelled status
        let mut session = ctx.session.clone();
        session.force_status(AgentStatus::Cancelled);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert!(ctx.is_cancelled());
    }

    #[test]
    fn test_agent_context_is_cancelled_true_when_session_missing() {
        let dir = make_test_dir("cancel-missing");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        // Don't insert session -- simulates a removed/expired session
        assert!(ctx.is_cancelled());
    }

    #[test]
    fn test_agent_context_persist_iteration() {
        let dir = make_test_dir("persist-iter");
        let stores = test_stores_with_dir(&dir);
        let (mut ctx, _rx) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        // Insert session with iteration 0
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(ctx.session.id.clone(), ctx.session.clone());

        // Bump iteration locally and persist
        ctx.session.iteration = 5;
        ctx.persist_iteration();

        // Verify stores reflect the update
        let sessions = stores.agent_sessions.read().unwrap();
        let stored = sessions.get(&ctx.session.id).unwrap();
        assert_eq!(stored.iteration, 5);
    }

    #[test]
    fn test_agent_context_persist_iteration_noop_when_missing() {
        let dir = make_test_dir("persist-iter-noop");
        let stores = test_stores_with_dir(&dir);
        let (mut ctx, _rx) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        // Don't insert session -- persist_iteration should silently no-op
        ctx.session.iteration = 10;
        ctx.persist_iteration(); // should not panic
    }

    #[test]
    fn test_agent_context_emit_iteration_completed() {
        let dir = make_test_dir("emit-iter");
        let stores = test_stores_with_dir(&dir);
        let (ctx, mut rx) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        ctx.emit_iteration_completed(3, "Planned 5 specs");

        // Verify the event was received
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "agent.iteration_completed");

        // Verify payload contents
        let data = event.data;
        assert_eq!(data["session_id"], ctx.session.id);
        assert_eq!(data["iteration"], 3);
        assert_eq!(data["summary"], "Planned 5 specs");
    }

    #[test]
    fn test_agent_context_emit_iteration_completed_no_receivers() {
        let dir = make_test_dir("emit-no-rx");
        let stores = test_stores_with_dir(&dir);
        let (ctx, rx) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        // Drop receiver -- emit should not panic (uses `let _ =`)
        drop(rx);
        ctx.emit_iteration_completed(1, "test summary");
    }

    #[test]
    fn test_cache_helper_returns_guard() {
        let dir = make_test_dir("cache-helper");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        // Basic usage: acquire lock, record, check hit
        let path = dir.join("test.txt");
        let mtime = std::time::SystemTime::UNIX_EPOCH;
        ctx.cache().record(&path, None, None, mtime, 42);
        let hit = ctx.cache().check_hit(&path, None, None, mtime);
        assert_eq!(hit, Some(42));
    }

    #[test]
    fn test_cache_helper_recovers_from_poison() {
        let dir = make_test_dir("cache-poison");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        // Poison the mutex by panicking in another thread while holding it
        let cache_ref = &ctx.read_cache;
        let result = std::thread::scope(|s| {
            s.spawn(|| {
                let _guard = cache_ref.lock().unwrap();
                panic!("intentional poison");
            })
            .join()
        });
        assert!(result.is_err(), "thread should have panicked");
        assert!(ctx.read_cache.is_poisoned(), "mutex should be poisoned");

        // cache() should recover without panicking
        let path = dir.join("after-poison.txt");
        let mtime = std::time::SystemTime::UNIX_EPOCH;
        ctx.cache().record(&path, None, None, mtime, 10);
        let hit = ctx.cache().check_hit(&path, None, None, mtime);
        assert_eq!(hit, Some(10));
    }
}
