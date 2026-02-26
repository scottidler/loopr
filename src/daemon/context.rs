use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};

use tokio::sync::{RwLock, broadcast};

use crate::config::Config;
use crate::domain::bundle::Bundle;
use crate::domain::learning::Learning;
use crate::domain::lock::Lock;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::spec::Spec;
use crate::domain::tick::Tick;
use crate::domain::work_item::WorkItem;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

/// In-memory record stores, each behind a std::sync::RwLock for synchronous access
/// from IPC request handlers (no async needed for in-memory HashMap operations).
pub struct Stores {
    pub plans: StdRwLock<HashMap<String, Plan>>,
    pub specs: StdRwLock<HashMap<String, Spec>>,
    pub phases: StdRwLock<HashMap<String, Phase>>,
    pub work_items: StdRwLock<HashMap<String, WorkItem>>,
    pub bundles: StdRwLock<HashMap<String, Bundle>>,
    pub ticks: StdRwLock<HashMap<String, Tick>>,
    pub learnings: StdRwLock<HashMap<String, Learning>>,
    pub locks: StdRwLock<HashMap<String, Lock>>,
}

impl Stores {
    pub fn new() -> Self {
        Self {
            plans: StdRwLock::new(HashMap::new()),
            specs: StdRwLock::new(HashMap::new()),
            phases: StdRwLock::new(HashMap::new()),
            work_items: StdRwLock::new(HashMap::new()),
            bundles: StdRwLock::new(HashMap::new()),
            ticks: StdRwLock::new(HashMap::new()),
            learnings: StdRwLock::new(HashMap::new()),
            locks: StdRwLock::new(HashMap::new()),
        }
    }
}

impl Default for Stores {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared state hub for the daemon.
/// All mutable state access goes through DaemonContext behind Arc<RwLock>.
pub struct DaemonContext {
    pub event_tx: broadcast::Sender<DaemonEvent>,
    pub config: Config,
    pub stores: Arc<Stores>,
    pub worktree_manager: WorktreeManager,
}

impl DaemonContext {
    /// Create a new DaemonContext with the given config and event broadcast channel.
    pub fn new(config: Config, event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        let repo_path = config.project.repo_path.clone();
        let worktree_dir = if config.project.worktree_dir.is_absolute() {
            config.project.worktree_dir.clone()
        } else {
            repo_path.join(&config.project.worktree_dir)
        };
        let worktree_manager = WorktreeManager::new(repo_path, worktree_dir);
        Self {
            config,
            event_tx,
            stores: Arc::new(Stores::new()),
            worktree_manager,
        }
    }

    /// Create a new DaemonContext wrapped in Arc<RwLock> for shared async access.
    pub fn shared(config: Config) -> (Arc<RwLock<Self>>, broadcast::Sender<DaemonEvent>) {
        let (event_tx, _) = broadcast::channel::<DaemonEvent>(256);
        let tx = event_tx.clone();
        let ctx = Self::new(config, event_tx);
        (Arc::new(RwLock::new(ctx)), tx)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_new() {
        let config = Config::default();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx);
        assert_eq!(ctx.config.name, "loopr");
        // Stores are initialized empty
        assert!(ctx.stores.plans.read().unwrap().is_empty());
    }

    #[test]
    fn test_context_shared() {
        let config = Config::default();
        let (ctx, tx) = DaemonContext::shared(config);
        // Can subscribe from the returned sender
        let _rx = tx.subscribe();
        // Can read from the context
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let c = ctx.read().await;
            assert_eq!(c.config.name, "loopr");
        });
    }

    #[test]
    fn test_context_event_broadcast() {
        let config = Config::default();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx);
        let mut rx = ctx.event_tx.subscribe();
        let event = DaemonEvent::record_created("plan", "p1");
        ctx.event_tx.send(event.clone()).unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received.event, "record.created");
    }

    #[test]
    fn test_context_shared_event_broadcast() {
        let config = Config::default();
        let (ctx, tx) = DaemonContext::shared(config);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let c = ctx.read().await;
            let mut rx = c.event_tx.subscribe();
            drop(c);
            tx.send(DaemonEvent::record_created("spec", "s1")).unwrap();
            let received = rx.try_recv().unwrap();
            assert_eq!(received.data["collection"], "spec");
        });
    }

    #[test]
    fn test_stores_default() {
        let stores = Stores::default();
        assert!(stores.plans.read().unwrap().is_empty());
        assert!(stores.specs.read().unwrap().is_empty());
        assert!(stores.phases.read().unwrap().is_empty());
        assert!(stores.work_items.read().unwrap().is_empty());
        assert!(stores.bundles.read().unwrap().is_empty());
        assert!(stores.ticks.read().unwrap().is_empty());
        assert!(stores.learnings.read().unwrap().is_empty());
        assert!(stores.locks.read().unwrap().is_empty());
    }

    #[test]
    fn test_stores_plan_insert_and_read() {
        let stores = Stores::new();
        let plan = Plan::new("Test".into(), "Desc".into(), "Criteria".into());
        let id = plan.id.clone();
        stores.plans.write().unwrap().insert(id.clone(), plan);
        let plans = stores.plans.read().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[&id].title, "Test");
    }

    #[test]
    fn test_stores_spec_insert_and_read() {
        let stores = Stores::new();
        let spec = Spec::new("plan-1".into(), "Test Spec".into(), "Desc".into());
        let id = spec.id.clone();
        stores.specs.write().unwrap().insert(id.clone(), spec);
        let specs = stores.specs.read().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[&id].title, "Test Spec");
    }

    #[test]
    fn test_stores_phase_insert_and_read() {
        let stores = Stores::new();
        let phase = Phase::new("spec-1".into(), "Test Phase".into(), "Desc".into(), 1);
        let id = phase.id.clone();
        stores.phases.write().unwrap().insert(id.clone(), phase);
        let phases = stores.phases.read().unwrap();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[&id].title, "Test Phase");
    }

    #[test]
    fn test_stores_work_item_insert_and_read() {
        let stores = Stores::new();
        let wi = WorkItem::new("phase-1".into(), "Test WI".into(), "Desc".into());
        let id = wi.id.clone();
        stores.work_items.write().unwrap().insert(id.clone(), wi);
        let work_items = stores.work_items.read().unwrap();
        assert_eq!(work_items.len(), 1);
        assert_eq!(work_items[&id].title, "Test WI");
    }

    #[test]
    fn test_stores_bundle_insert_and_read() {
        let stores = Stores::new();
        let bundle = Bundle::new(
            "wi-1".into(),
            Some("tick-1".into()),
            "feature/test".into(),
            "Test claims".into(),
        );
        let id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(id.clone(), bundle);
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[&id].branch_name, "feature/test");
    }

    #[test]
    fn test_stores_tick_insert_and_read() {
        let stores = Stores::new();
        let tick = Tick::new(1);
        let id = tick.id.clone();
        stores.ticks.write().unwrap().insert(id.clone(), tick);
        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[&id].number, 1);
    }

    #[test]
    fn test_stores_lock_insert_and_read() {
        let stores = Stores::new();
        let lock = crate::domain::lock::Lock::new("src/main.rs".into(), "wi-1".into(), "coord-1".into());
        let id = lock.id.clone();
        stores.locks.write().unwrap().insert(id.clone(), lock);
        let locks = stores.locks.read().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[&id].resource, "src/main.rs");
    }

    #[test]
    fn test_stores_learning_insert_and_read() {
        let stores = Stores::new();
        let learning = Learning::new(
            "wi-1".into(),
            crate::domain::learning::LearningScope::WorkItem,
            "Test insight".into(),
        );
        let id = learning.id.clone();
        stores.learnings.write().unwrap().insert(id.clone(), learning);
        let learnings = stores.learnings.read().unwrap();
        assert_eq!(learnings.len(), 1);
        assert_eq!(learnings[&id].content, "Test insight");
    }
}
