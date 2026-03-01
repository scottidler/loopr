use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};

use log::{debug, info, warn};
use taskstore::Store;
use tokio::sync::{RwLock, broadcast};

use tokio::task::JoinHandle;

use crate::agents::{AgentEvent, AgentSession, AgentStatus};
use crate::config::Config;
use crate::domain::bundle::{Bundle, BundleStatus};
use crate::domain::coordinator_goal::CoordinatorGoal;
use crate::domain::coordinator_state::CoordinatorState;
use crate::domain::decision::Decision;
use crate::domain::learning::Learning;
use crate::domain::lock::Lock;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::proposal::Proposal;
use crate::domain::spec::Spec;
use crate::domain::tick::{Tick, TickStatus};
use crate::domain::validation::ValidationReport;
use crate::domain::work::{Work, WorkStatus};
use crate::ipc::protocol::DaemonEvent;
use crate::tools::ToolRunner;
use crate::validator::DocValidator;
use crate::worktree::manager::WorktreeManager;

/// In-memory record stores, each behind a std::sync::RwLock for synchronous access
/// from IPC request handlers (no async needed for in-memory HashMap operations).
pub struct Stores {
    pub plans: StdRwLock<HashMap<String, Plan>>,
    pub specs: StdRwLock<HashMap<String, Spec>>,
    pub phases: StdRwLock<HashMap<String, Phase>>,
    pub works: StdRwLock<HashMap<String, Work>>,
    pub bundles: StdRwLock<HashMap<String, Bundle>>,
    pub ticks: StdRwLock<HashMap<String, Tick>>,
    pub learnings: StdRwLock<HashMap<String, Learning>>,
    pub locks: StdRwLock<HashMap<String, Lock>>,
    pub coordinator_goals: StdRwLock<HashMap<String, CoordinatorGoal>>,
    pub coordinator_states: StdRwLock<HashMap<String, CoordinatorState>>,
    pub proposals: StdRwLock<HashMap<String, Proposal>>,
    pub decisions: StdRwLock<HashMap<String, Decision>>,
    pub agent_sessions: StdRwLock<HashMap<String, AgentSession>>,
    /// TaskStore for persistent JSONL+SQLite storage. None in legacy/test contexts.
    pub store: Option<Arc<StdMutex<Store>>>,
    /// Doc Validator (LLM-based). None when validator.enabled = false or in legacy contexts.
    pub validator: Option<Arc<DocValidator>>,
    /// Tool runner for agent subprocess execution. Shared across agent tasks.
    pub tool_runner: Arc<ToolRunner>,
    /// Full config, available to handlers for agent spawning.
    pub config: Config,
    /// JoinHandles for spawned agent tasks, keyed by session ID.
    /// Used for graceful shutdown: cancel agents, wait, then abort.
    pub agent_handles: StdMutex<HashMap<String, JoinHandle<()>>>,
    /// Per-session ring buffer of agent events for agent.output IPC method.
    pub agent_events: StdRwLock<HashMap<String, VecDeque<AgentEvent>>>,
    /// Fix #10: Advisory lock for main repo git operations (merge, reset).
    /// Prevents concurrent Integrator merges from racing.
    pub git_lock: StdMutex<()>,
}

impl Stores {
    pub fn new() -> Self {
        Self {
            plans: StdRwLock::new(HashMap::new()),
            specs: StdRwLock::new(HashMap::new()),
            phases: StdRwLock::new(HashMap::new()),
            works: StdRwLock::new(HashMap::new()),
            bundles: StdRwLock::new(HashMap::new()),
            ticks: StdRwLock::new(HashMap::new()),
            learnings: StdRwLock::new(HashMap::new()),
            locks: StdRwLock::new(HashMap::new()),
            coordinator_goals: StdRwLock::new(HashMap::new()),
            coordinator_states: StdRwLock::new(HashMap::new()),
            proposals: StdRwLock::new(HashMap::new()),
            decisions: StdRwLock::new(HashMap::new()),
            agent_sessions: StdRwLock::new(HashMap::new()),
            store: None,
            validator: None,
            tool_runner: Arc::new(ToolRunner::new(&[])),
            config: Config::default(),
            agent_handles: StdMutex::new(HashMap::new()),
            agent_events: StdRwLock::new(HashMap::new()),
            git_lock: StdMutex::new(()),
        }
    }
}

/// Record an agent event in the per-session ring buffer.
pub fn record_agent_event(stores: &Stores, session_id: &str, event: &AgentEvent) {
    let mut events = stores.agent_events.write().unwrap();
    let ring = events
        .entry(session_id.to_string())
        .or_insert_with(|| VecDeque::with_capacity(1000));
    if ring.len() >= 1000 {
        ring.pop_front();
    }
    ring.push_back(event.clone());
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
    /// Opens the TaskStore at the project repo_path and rebuilds indexes for all domain types.
    pub fn new(config: Config, event_tx: broadcast::Sender<DaemonEvent>) -> eyre::Result<Self> {
        let repo_path = config.project.repo_path.clone();
        let worktree_dir = if config.project.worktree_dir.is_absolute() {
            config.project.worktree_dir.clone()
        } else {
            repo_path.join(&config.project.worktree_dir)
        };
        let worktree_manager = WorktreeManager::new(repo_path.clone(), worktree_dir);

        // Open TaskStore at repo_path (creates .taskstore/ subdirectory)
        let mut store = Store::open(&repo_path)?;

        // Rebuild indexes for all domain types so queries work after JSONL sync
        store.rebuild_indexes::<Plan>()?;
        store.rebuild_indexes::<Spec>()?;
        store.rebuild_indexes::<Phase>()?;
        store.rebuild_indexes::<Work>()?;
        store.rebuild_indexes::<Bundle>()?;
        store.rebuild_indexes::<Tick>()?;
        store.rebuild_indexes::<Learning>()?;
        store.rebuild_indexes::<Lock>()?;
        store.rebuild_indexes::<CoordinatorGoal>()?;
        store.rebuild_indexes::<CoordinatorState>()?;
        store.rebuild_indexes::<Proposal>()?;
        store.rebuild_indexes::<Decision>()?;
        store.rebuild_indexes::<ValidationReport>()?;
        store.rebuild_indexes::<AgentSession>()?;

        info!("TaskStore opened at {}", repo_path.display());

        let mut stores = Stores::new();

        // Hydrate in-memory HashMaps from TaskStore so all code paths
        // (parent validation, crash recovery, transitions) work after restart.
        {
            for plan in store.list::<Plan>(&[])? {
                stores.plans.write().unwrap().insert(plan.id.clone(), plan);
            }
            for spec in store.list::<Spec>(&[])? {
                stores.specs.write().unwrap().insert(spec.id.clone(), spec);
            }
            for phase in store.list::<Phase>(&[])? {
                stores.phases.write().unwrap().insert(phase.id.clone(), phase);
            }
            for wi in store.list::<Work>(&[])? {
                stores.works.write().unwrap().insert(wi.id.clone(), wi);
            }
            for bundle in store.list::<Bundle>(&[])? {
                stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);
            }
            for tick in store.list::<Tick>(&[])? {
                stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
            }
            for learning in store.list::<Learning>(&[])? {
                stores.learnings.write().unwrap().insert(learning.id.clone(), learning);
            }
            for lock in store.list::<Lock>(&[])? {
                stores.locks.write().unwrap().insert(lock.id.clone(), lock);
            }
            for goal in store.list::<CoordinatorGoal>(&[])? {
                stores.coordinator_goals.write().unwrap().insert(goal.id.clone(), goal);
            }
            for cs in store.list::<CoordinatorState>(&[])? {
                stores.coordinator_states.write().unwrap().insert(cs.id.clone(), cs);
            }
            for proposal in store.list::<Proposal>(&[])? {
                stores.proposals.write().unwrap().insert(proposal.id.clone(), proposal);
            }
            for decision in store.list::<Decision>(&[])? {
                stores.decisions.write().unwrap().insert(decision.id.clone(), decision);
            }
            for session in store.list::<AgentSession>(&[])? {
                stores
                    .agent_sessions
                    .write()
                    .unwrap()
                    .insert(session.id.clone(), session);
            }
            let hydrated: usize = stores.plans.read().unwrap().len()
                + stores.specs.read().unwrap().len()
                + stores.phases.read().unwrap().len()
                + stores.works.read().unwrap().len()
                + stores.bundles.read().unwrap().len()
                + stores.ticks.read().unwrap().len()
                + stores.learnings.read().unwrap().len()
                + stores.locks.read().unwrap().len()
                + stores.coordinator_goals.read().unwrap().len()
                + stores.coordinator_states.read().unwrap().len()
                + stores.proposals.read().unwrap().len()
                + stores.decisions.read().unwrap().len()
                + stores.agent_sessions.read().unwrap().len();
            if hydrated > 0 {
                info!("Hydrated {} records from TaskStore into memory", hydrated);
            }
        }

        stores.store = Some(Arc::new(StdMutex::new(store)));

        // Store config for handler access (agent spawning, etc.)
        stores.config = config.clone();

        // Create ToolRunner from agent config
        stores.tool_runner = Arc::new(ToolRunner::new(&config.agents.tools));
        info!(
            "Tool runner initialized with {} tools",
            stores.tool_runner.available_tools().len()
        );

        // Create DocValidator if enabled in config
        if config.validator.enabled {
            info!(
                "Doc Validator enabled: provider={}, model={}",
                config.validator.provider, config.validator.model
            );
            stores.validator = Some(Arc::new(DocValidator::new(config.validator.clone())));
        } else {
            info!("Doc Validator disabled");
        }

        Ok(Self {
            config,
            event_tx,
            stores: Arc::new(stores),
            worktree_manager,
        })
    }

    /// Recover orphaned records after a crash.
    ///
    /// On daemon startup (especially after crash recovery from persistent storage),
    /// this scans for records stuck in transient states:
    /// - InProgress Works → reset to Blocked
    /// - Integrating Bundles → reset to Accepted
    ///
    /// Returns the number of records recovered.
    pub fn recover_orphaned_records(&self) -> usize {
        let mut recovered = 0;

        // Recover InProgress Works → Blocked
        {
            let mut works = self.stores.works.write().unwrap();
            let store_lock = self.stores.store.as_ref();
            for (id, wi) in works.iter_mut() {
                if wi.status == WorkStatus::InProgress {
                    warn!("Recovering orphaned InProgress Work: {}", id);
                    wi.status = WorkStatus::Blocked;
                    wi.updated_at = crate::id::now_millis();
                    // Persist recovery to TaskStore
                    if let Some(store_arc) = store_lock
                        && let Err(e) = store_arc.lock().unwrap().update(wi.clone())
                    {
                        warn!("Failed to persist Work recovery to TaskStore: {}", e);
                    }
                    recovered += 1;
                }
            }
        }

        // Recover Integrating Bundles → Accepted
        {
            let mut bundles = self.stores.bundles.write().unwrap();
            let store_lock = self.stores.store.as_ref();
            for (id, bundle) in bundles.iter_mut() {
                if bundle.status == BundleStatus::Integrating {
                    warn!("Recovering orphaned Integrating Bundle: {}", id);
                    bundle.status = BundleStatus::Accepted;
                    bundle.updated_at = crate::id::now_millis();
                    // Persist recovery to TaskStore
                    if let Some(store_arc) = store_lock
                        && let Err(e) = store_arc.lock().unwrap().update(bundle.clone())
                    {
                        warn!("Failed to persist Bundle recovery to TaskStore: {}", e);
                    }
                    recovered += 1;
                }
            }
        }

        // Recover stuck Ticks (Open/Sealing/Validating) → Failed
        {
            let mut ticks = self.stores.ticks.write().unwrap();
            let store_lock = self.stores.store.as_ref();
            for (id, tick) in ticks.iter_mut() {
                if tick.status == TickStatus::Open
                    || tick.status == TickStatus::Sealing
                    || tick.status == TickStatus::Validating
                {
                    warn!("Recovering stuck Tick in {:?} state: {}", tick.status, id);
                    tick.status = TickStatus::Failed;
                    tick.updated_at = crate::id::now_millis();
                    // Persist recovery to TaskStore
                    if let Some(store_arc) = store_lock
                        && let Err(e) = store_arc.lock().unwrap().update(tick.clone())
                    {
                        warn!("Failed to persist Tick recovery to TaskStore: {}", e);
                    }
                    recovered += 1;
                }
            }
        }

        // Recover stuck AgentSessions (non-terminal) → Failed
        {
            let mut sessions = self.stores.agent_sessions.write().unwrap();
            let store_lock = self.stores.store.as_ref();
            for (id, session) in sessions.iter_mut() {
                if !session.status.is_terminal() {
                    warn!("Recovering stuck AgentSession in {:?} state: {}", session.status, id);
                    // Use direct mutation — transition_to may fail for some paths
                    // (e.g., Starting → Failed is valid, but we want to force recovery)
                    session.status = AgentStatus::Failed;
                    session.error_message = Some("Recovered after daemon crash".to_string());
                    session.updated_at = crate::id::now_millis();
                    if let Some(store_arc) = store_lock
                        && let Err(e) = store_arc.lock().unwrap().update(session.clone())
                    {
                        warn!("Failed to persist AgentSession recovery to TaskStore: {}", e);
                    }
                    recovered += 1;
                }
            }
        }

        // Gap #30: Expire stale locks
        {
            let mut locks = self.stores.locks.write().unwrap();
            let store_lock = self.stores.store.as_ref();
            for (id, lock) in locks.iter_mut() {
                if lock.is_active() && lock.is_expired() {
                    warn!("Recovering expired Lock: {} (resource={})", id, lock.resource);
                    lock.expire();
                    if let Some(store_arc) = store_lock
                        && let Err(e) = store_arc.lock().unwrap().update(lock.clone())
                    {
                        warn!("Failed to persist Lock recovery to TaskStore: {}", e);
                    }
                    recovered += 1;
                }
            }
        }

        if recovered > 0 {
            info!("Crash recovery: reset {} orphaned record(s)", recovered);
        } else {
            info!("Crash recovery: no orphaned records found");
        }

        recovered
    }

    /// Create a new DaemonContext wrapped in Arc<RwLock> for shared async access.
    pub fn shared(config: Config) -> eyre::Result<(Arc<RwLock<Self>>, broadcast::Sender<DaemonEvent>)> {
        debug!("DaemonContext::shared()");
        let (event_tx, _) = broadcast::channel::<DaemonEvent>(256);
        let tx = event_tx.clone();
        let ctx = Self::new(config, event_tx)?;
        Ok((Arc::new(RwLock::new(ctx)), tx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentType;
    use crate::config::ProjectConfig;

    /// Create a test Config with repo_path pointing to a unique temp directory
    /// so TaskStore doesn't pollute the project or collide between tests.
    fn test_config() -> Config {
        let id = crate::id::generate_id();
        let dir = std::env::temp_dir().join(format!("loopr-ctx-test-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        Config {
            project: ProjectConfig {
                repo_path: dir,
                ..ProjectConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn test_context_new() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();
        assert_eq!(ctx.config.name, "loopr");
        // Fresh TaskStore has no records, so HashMaps are empty after hydration
        assert!(ctx.stores.plans.read().unwrap().is_empty());
    }

    #[test]
    fn test_context_hydrates_from_taskstore() {
        let config = test_config();
        let repo_path = config.project.repo_path.clone();

        // First: create records directly via TaskStore
        {
            let mut store = Store::open(&repo_path).unwrap();
            let plan = Plan::new("Hydration Test".into(), "Desc".into(), "Criteria".into());
            store.create(plan).unwrap();
            let spec = Spec::new("plan-1".into(), "Spec".into(), "Desc".into());
            store.create(spec).unwrap();
        }

        // Second: open DaemonContext — should hydrate HashMaps from TaskStore
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();
        assert_eq!(ctx.stores.plans.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.specs.read().unwrap().len(), 1);
    }

    #[test]
    fn test_context_new_creates_taskstore() {
        let config = test_config();
        let repo_path = config.project.repo_path.clone();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        // TaskStore should have created .taskstore/ directory
        assert!(repo_path.join(".taskstore").exists());

        // Store should be accessible via the Arc<Mutex>
        let store = ctx.stores.store.as_ref().unwrap().lock().unwrap();
        // Listing empty collections should return empty vecs
        let plans: Vec<Plan> = store.list(&[]).unwrap();
        assert!(plans.is_empty());
    }

    #[test]
    fn test_context_taskstore_crud() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        // Create a plan via TaskStore
        let plan = Plan::new("Test Plan".into(), "Description".into(), "Criteria".into());
        let plan_id = plan.id.clone();
        ctx.stores.store.as_ref().unwrap().lock().unwrap().create(plan).unwrap();

        // Get it back
        let retrieved: Option<Plan> = ctx
            .stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .get(&plan_id)
            .unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Test Plan");

        // List should return it
        let all: Vec<Plan> = ctx.stores.store.as_ref().unwrap().lock().unwrap().list(&[]).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_context_shared() {
        let config = test_config();
        let (ctx, tx) = DaemonContext::shared(config).unwrap();
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
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();
        let mut rx = ctx.event_tx.subscribe();
        let event = DaemonEvent::record_created("plan", "p1");
        ctx.event_tx.send(event.clone()).unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received.event, "record.created");
    }

    #[test]
    fn test_context_shared_event_broadcast() {
        let config = test_config();
        let (ctx, tx) = DaemonContext::shared(config).unwrap();
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
        assert!(stores.works.read().unwrap().is_empty());
        assert!(stores.bundles.read().unwrap().is_empty());
        assert!(stores.ticks.read().unwrap().is_empty());
        assert!(stores.learnings.read().unwrap().is_empty());
        assert!(stores.locks.read().unwrap().is_empty());
        assert!(stores.coordinator_goals.read().unwrap().is_empty());
        assert!(stores.agent_sessions.read().unwrap().is_empty());
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
    fn test_stores_work_insert_and_read() {
        let stores = Stores::new();
        let wi = Work::new("phase-1".into(), "Test WI".into(), "Desc".into());
        let id = wi.id.clone();
        stores.works.write().unwrap().insert(id.clone(), wi);
        let works = stores.works.read().unwrap();
        assert_eq!(works.len(), 1);
        assert_eq!(works[&id].title, "Test WI");
    }

    #[test]
    fn test_stores_bundle_insert_and_read() {
        let stores = Stores::new();
        let bundle = Bundle::new(
            "wi-1".into(),
            Some("tick-1".into()),
            "feature/test".into(),
            vec!["Test claims".into()],
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
            crate::domain::learning::LearningScope::Work,
            "Test insight".into(),
        );
        let id = learning.id.clone();
        stores.learnings.write().unwrap().insert(id.clone(), learning);
        let learnings = stores.learnings.read().unwrap();
        assert_eq!(learnings.len(), 1);
        assert_eq!(learnings[&id].content, "Test insight");
    }

    #[test]
    fn test_recover_orphaned_works() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        // Insert a Work in InProgress state (orphaned)
        let mut wi = Work::new("phase-1".into(), "Orphaned WI".into(), "".into());
        wi.status = WorkStatus::InProgress;
        let wi_id = wi.id.clone();
        ctx.stores.works.write().unwrap().insert(wi_id.clone(), wi);

        // Insert a Work in Draft state (not orphaned)
        let wi2 = Work::new("phase-1".into(), "Normal WI".into(), "".into());
        let wi2_id = wi2.id.clone();
        ctx.stores.works.write().unwrap().insert(wi2_id.clone(), wi2);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 1);

        let works = ctx.stores.works.read().unwrap();
        assert_eq!(works[&wi_id].status, WorkStatus::Blocked);
        assert_eq!(works[&wi2_id].status, WorkStatus::Draft);
    }

    #[test]
    fn test_recover_orphaned_bundles() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        // Insert a Bundle in Integrating state (orphaned)
        let mut bundle = Bundle::new(
            "wi-1".into(),
            Some("tick-1".into()),
            "feature/orphaned".into(),
            vec!["claims".into()],
        );
        bundle.status = BundleStatus::Integrating;
        let b_id = bundle.id.clone();
        ctx.stores.bundles.write().unwrap().insert(b_id.clone(), bundle);

        // Insert a Bundle in Proposed state (not orphaned)
        let bundle2 = Bundle::new(
            "wi-2".into(),
            Some("tick-1".into()),
            "feature/normal".into(),
            vec!["claims".into()],
        );
        let b2_id = bundle2.id.clone();
        ctx.stores.bundles.write().unwrap().insert(b2_id.clone(), bundle2);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 1);

        let bundles = ctx.stores.bundles.read().unwrap();
        assert_eq!(bundles[&b_id].status, BundleStatus::Accepted);
        assert_eq!(bundles[&b2_id].status, BundleStatus::Proposed);
    }

    #[test]
    fn test_recover_both_orphaned_types() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        let mut wi = Work::new("phase-1".into(), "Orphaned WI".into(), "".into());
        wi.status = WorkStatus::InProgress;
        ctx.stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut bundle = Bundle::new(
            "wi-1".into(),
            Some("tick-1".into()),
            "feature/orphaned".into(),
            vec!["claims".into()],
        );
        bundle.status = BundleStatus::Integrating;
        ctx.stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 2);
    }

    #[test]
    fn test_recover_no_orphans() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        // Draft Work — not orphaned
        let wi = Work::new("phase-1".into(), "Normal WI".into(), "".into());
        ctx.stores.works.write().unwrap().insert(wi.id.clone(), wi);

        // Proposed Bundle — not orphaned
        let bundle = Bundle::new("wi-1".into(), None, "feature/ok".into(), vec!["claims".into()]);
        ctx.stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 0);
    }

    #[test]
    fn test_recover_empty_stores() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();
        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 0);
    }

    #[test]
    fn test_recover_stuck_tick_sealing() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Sealing;
        let tick_id = tick.id.clone();
        ctx.stores.ticks.write().unwrap().insert(tick_id.clone(), tick);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 1);

        let ticks = ctx.stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
    }

    #[test]
    fn test_recover_stuck_tick_validating() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        let mut tick = Tick::new(2);
        tick.status = TickStatus::Validating;
        let tick_id = tick.id.clone();
        ctx.stores.ticks.write().unwrap().insert(tick_id.clone(), tick);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 1);

        let ticks = ctx.stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
    }

    #[test]
    fn test_recover_open_tick_recovered() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        let tick = Tick::new(3);
        let tick_id = tick.id.clone();
        ctx.stores.ticks.write().unwrap().insert(tick_id.clone(), tick);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 1);

        let ticks = ctx.stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
    }

    #[test]
    fn test_recover_mixed_orphans() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        // Orphaned InProgress Work
        let mut wi = Work::new("phase-1".into(), "Orphaned WI".into(), "".into());
        wi.status = WorkStatus::InProgress;
        ctx.stores.works.write().unwrap().insert(wi.id.clone(), wi);

        // Stuck Sealing Tick
        let mut tick = Tick::new(1);
        tick.status = TickStatus::Sealing;
        ctx.stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        // Orphaned Integrating Bundle
        let mut bundle = Bundle::new(
            "wi-1".into(),
            Some("tick-1".into()),
            "feature/orphaned".into(),
            vec!["claims".into()],
        );
        bundle.status = BundleStatus::Integrating;
        ctx.stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 3);
    }

    #[test]
    fn test_recover_stuck_session_running() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        let mut session = AgentSession::new(AgentType::Implementer, "test-model".into());
        session.status = AgentStatus::Running;
        let session_id = session.id.clone();
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let recovered = ctx.recover_orphaned_records();
        assert!(recovered >= 1);

        let sessions = ctx.stores.agent_sessions.read().unwrap();
        assert_eq!(sessions[&session_id].status, AgentStatus::Failed);
        assert!(sessions[&session_id].error_message.as_ref().unwrap().contains("crash"));
    }

    #[test]
    fn test_recover_stuck_session_waiting_for_llm() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        let mut session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        session.status = AgentStatus::Running;
        session.transition_to(AgentStatus::WaitingForLlm).unwrap();
        let session_id = session.id.clone();
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let recovered = ctx.recover_orphaned_records();
        assert!(recovered >= 1);

        let sessions = ctx.stores.agent_sessions.read().unwrap();
        assert_eq!(sessions[&session_id].status, AgentStatus::Failed);
    }

    #[test]
    fn test_recover_completed_session_not_touched() {
        let config = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx).unwrap();

        let mut session = AgentSession::new(AgentType::Implementer, "test-model".into());
        session.status = AgentStatus::Completed;
        let session_id = session.id.clone();
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 0);

        let sessions = ctx.stores.agent_sessions.read().unwrap();
        assert_eq!(sessions[&session_id].status, AgentStatus::Completed);
    }

    #[test]
    fn test_stores_coordinator_goal_insert_and_read() {
        let stores = Stores::new();
        let goal = CoordinatorGoal::new("Build auth system".into());
        let id = goal.id.clone();
        stores.coordinator_goals.write().unwrap().insert(id.clone(), goal);
        let goals = stores.coordinator_goals.read().unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[&id].goal, "Build auth system");
        assert!(goals[&id].active);
    }
}
