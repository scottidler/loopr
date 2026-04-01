use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, RwLock as StdRwLock, RwLockReadGuard, RwLockWriteGuard};

use eyre::{Result, eyre};
use log::{debug, info, warn};
use paste::paste;
use taskstore::Store;
use tokio::sync::{RwLock, broadcast};

use tokio::task::JoinHandle;

use crate::agents::{AgentEvent, AgentSession, AgentStatus};
use crate::config::Config;
use crate::config::ToolEntry;
use crate::domain::bundle::{Bundle, BundleStatus};
use crate::domain::chat::ChatHistory;
use crate::domain::coordinator_goal::CoordinatorGoal;
use crate::domain::coordinator_state::CoordinatorState;
use crate::domain::coverage::CoverageReport;
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
use crate::guidance::AgentGuidance;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::ToolExecutor;
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
    pub coverage_reports: StdRwLock<HashMap<String, CoverageReport>>,
    /// TaskStore for persistent JSONL+SQLite storage. None in legacy/test contexts.
    pub store: Option<Arc<StdMutex<Store>>>,
    /// Doc Validator (LLM-based). None when validator.enabled = false or in legacy contexts.
    pub validator: Option<Arc<DocValidator>>,
    /// Coverage Evaluator (LLM-based). None when coverage is disabled or in legacy contexts.
    pub evaluator: Option<Arc<crate::evaluator::CoverageEvaluator>>,
    /// Runtime tools discovered by agents via tools.register IPC. Session-scoped, not persisted.
    pub runtime_tools: StdRwLock<HashMap<String, ToolEntry>>,
    /// Tool runner for agent subprocess execution. Wrapped in RwLock for atomic swap on tools.register.
    pub tool_runner: StdRwLock<Arc<ToolRunner>>,
    /// Unified tool executor (built-in + configured tools). Wrapped in RwLock for atomic swap on tools.register.
    pub tool_executor: StdRwLock<Arc<ToolExecutor>>,
    /// Full config, available to handlers for agent spawning.
    pub config: Config,
    /// Assembled guidance (schema docs + LOOPR.md files), loaded once at startup.
    pub guidance: AgentGuidance,
    /// JoinHandles for spawned agent tasks, keyed by session ID.
    /// Used for graceful shutdown: cancel agents, wait, then abort.
    pub agent_handles: StdMutex<HashMap<String, JoinHandle<()>>>,
    /// Per-session ring buffer of agent events for agent.output IPC method.
    pub agent_events: StdRwLock<HashMap<String, VecDeque<AgentEvent>>>,
    /// Fix #10: Advisory lock for main repo git operations (merge, reset).
    /// Prevents concurrent Integrator merges from racing.
    pub git_lock: StdMutex<()>,
    /// Signal for graceful shutdown of persistent workers.
    pub shutting_down: AtomicBool,
    /// Session directory for this daemon run. Used by AgentLogger for scoped log output.
    pub session_dir: Option<std::path::PathBuf>,
    /// Chat session conversation histories, keyed by session ID (e.g., "default-chat").
    pub chat_sessions: StdRwLock<HashMap<String, ChatHistory>>,
    /// Daemon session ID (timestamp-based), set once at startup.
    pub session_id: String,
}

macro_rules! store_accessors {
    ($($field:ident : $value_type:ty),* $(,)?) => {
        $(
            paste! {
                pub fn [<read_ $field>](&self) -> Result<RwLockReadGuard<'_, HashMap<String, $value_type>>> {
                    self.$field.read().map_err(|_| eyre!(concat!(stringify!($field), " lock poisoned")))
                }
                pub fn [<write_ $field>](&self) -> Result<RwLockWriteGuard<'_, HashMap<String, $value_type>>> {
                    self.$field.write().map_err(|_| eyre!(concat!(stringify!($field), " lock poisoned")))
                }
            }
        )*
    };
}

impl Stores {
    store_accessors! {
        plans: Plan,
        specs: Spec,
        phases: Phase,
        works: Work,
        bundles: Bundle,
        ticks: Tick,
        learnings: Learning,
        locks: Lock,
        coordinator_goals: CoordinatorGoal,
        coordinator_states: CoordinatorState,
        proposals: Proposal,
        decisions: Decision,
        agent_sessions: AgentSession,
        coverage_reports: CoverageReport,
        agent_events: VecDeque<AgentEvent>,
        runtime_tools: ToolEntry,
    }

    /// Clone the current Arc<ToolRunner> from behind the RwLock.
    pub fn read_tool_runner(&self) -> Result<Arc<ToolRunner>> {
        Ok(self
            .tool_runner
            .read()
            .map_err(|_| eyre!("tool_runner lock poisoned"))?
            .clone())
    }

    /// Clone the current Arc<ToolExecutor> from behind the RwLock.
    pub fn read_tool_executor(&self) -> Result<Arc<ToolExecutor>> {
        Ok(self
            .tool_executor
            .read()
            .map_err(|_| eyre!("tool_executor lock poisoned"))?
            .clone())
    }

    pub fn lock_store(&self) -> Result<Option<MutexGuard<'_, Store>>> {
        match &self.store {
            Some(s) => Ok(Some(s.lock().map_err(|_| eyre!("taskstore lock poisoned"))?)),
            None => Ok(None),
        }
    }

    pub fn lock_store_required(&self) -> Result<MutexGuard<'_, Store>> {
        let store = self.store.as_ref().ok_or_else(|| eyre!("TaskStore not initialized"))?;
        store.lock().map_err(|_| eyre!("taskstore lock poisoned"))
    }

    pub fn lock_agent_handles(&self) -> Result<MutexGuard<'_, HashMap<String, JoinHandle<()>>>> {
        self.agent_handles
            .lock()
            .map_err(|_| eyre!("agent_handles lock poisoned"))
    }

    pub fn lock_git(&self) -> Result<MutexGuard<'_, ()>> {
        self.git_lock.lock().map_err(|_| eyre!("git_lock poisoned"))
    }
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
            coverage_reports: StdRwLock::new(HashMap::new()),
            runtime_tools: StdRwLock::new(HashMap::new()),
            store: None,
            validator: None,
            evaluator: None,
            tool_runner: StdRwLock::new(Arc::new(ToolRunner::new(&[]))),
            tool_executor: StdRwLock::new(Arc::new(ToolExecutor::standard(&[]))),
            config: Config::default(),
            agent_handles: StdMutex::new(HashMap::new()),
            agent_events: StdRwLock::new(HashMap::new()),
            git_lock: StdMutex::new(()),
            shutting_down: AtomicBool::new(false),
            guidance: AgentGuidance::schema_only(),
            session_dir: None,
            chat_sessions: StdRwLock::new(HashMap::new()),
            session_id: String::new(),
        }
    }
}

/// Record an agent event in the per-session ring buffer.
pub fn record_agent_event(stores: &Stores, session_id: &str, event: &AgentEvent) {
    let Ok(mut events) = stores.write_agent_events() else {
        log::error!("agent_events lock poisoned, dropping event for session {session_id}");
        return;
    };
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
    pub session_id: String,
    pub session_dir: std::path::PathBuf,
}

impl DaemonContext {
    /// Create a new DaemonContext with the given config and event broadcast channel.
    /// Opens the TaskStore at the project repo_path and rebuilds indexes for all domain types.
    pub fn new(
        config: Config,
        event_tx: broadcast::Sender<DaemonEvent>,
        session_id: String,
        session_dir: std::path::PathBuf,
    ) -> eyre::Result<Self> {
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
        store.rebuild_indexes::<CoverageReport>()?;
        store.rebuild_indexes::<AgentSession>()?;

        info!("TaskStore opened at {}", repo_path.display());

        let mut stores = Stores::new();

        // Hydrate in-memory HashMaps from TaskStore so all code paths
        // (parent validation, crash recovery, transitions) work after restart.
        {
            for plan in store.list::<Plan>(&[])? {
                stores.write_plans()?.insert(plan.id.clone(), plan);
            }
            for spec in store.list::<Spec>(&[])? {
                stores.write_specs()?.insert(spec.id.clone(), spec);
            }
            for phase in store.list::<Phase>(&[])? {
                stores.write_phases()?.insert(phase.id.clone(), phase);
            }
            for wi in store.list::<Work>(&[])? {
                stores.write_works()?.insert(wi.id.clone(), wi);
            }
            for bundle in store.list::<Bundle>(&[])? {
                stores.write_bundles()?.insert(bundle.id.clone(), bundle);
            }
            for tick in store.list::<Tick>(&[])? {
                stores.write_ticks()?.insert(tick.id.clone(), tick);
            }
            for learning in store.list::<Learning>(&[])? {
                stores.write_learnings()?.insert(learning.id.clone(), learning);
            }
            for lock in store.list::<Lock>(&[])? {
                stores.write_locks()?.insert(lock.id.clone(), lock);
            }
            for goal in store.list::<CoordinatorGoal>(&[])? {
                stores.write_coordinator_goals()?.insert(goal.id.clone(), goal);
            }
            for cs in store.list::<CoordinatorState>(&[])? {
                stores.write_coordinator_states()?.insert(cs.id.clone(), cs);
            }
            for proposal in store.list::<Proposal>(&[])? {
                stores.write_proposals()?.insert(proposal.id.clone(), proposal);
            }
            for decision in store.list::<Decision>(&[])? {
                stores.write_decisions()?.insert(decision.id.clone(), decision);
            }
            for cr in store.list::<CoverageReport>(&[])? {
                stores.write_coverage_reports()?.insert(cr.id.clone(), cr);
            }
            for session in store.list::<AgentSession>(&[])? {
                stores.write_agent_sessions()?.insert(session.id.clone(), session);
            }
            let hydrated: usize = stores.read_plans()?.len()
                + stores.read_specs()?.len()
                + stores.read_phases()?.len()
                + stores.read_works()?.len()
                + stores.read_bundles()?.len()
                + stores.read_ticks()?.len()
                + stores.read_learnings()?.len()
                + stores.read_locks()?.len()
                + stores.read_coordinator_goals()?.len()
                + stores.read_coordinator_states()?.len()
                + stores.read_proposals()?.len()
                + stores.read_decisions()?.len()
                + stores.read_coverage_reports()?.len()
                + stores.read_agent_sessions()?.len();
            if hydrated > 0 {
                info!("Hydrated {} records from TaskStore into memory", hydrated);
            }
        }

        stores.store = Some(Arc::new(StdMutex::new(store)));

        // Store config for handler access (agent spawning, etc.)
        stores.config = config.clone();
        stores.session_dir = Some(session_dir.clone());

        // Create ToolRunner from agent config
        stores.tool_runner = StdRwLock::new(Arc::new(ToolRunner::new(&config.agents.tools)));
        stores.tool_executor = StdRwLock::new(Arc::new(ToolExecutor::standard(&config.agents.tools)));
        info!(
            "Tool runner initialized with {} tools",
            stores.read_tool_runner()?.available_tools().len()
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

        // Create CoverageEvaluator if enabled in config
        if config.evaluator.enabled {
            info!(
                "Coverage Evaluator enabled: provider={}, model={}",
                config.evaluator.provider, config.evaluator.model
            );
            let eval_config = crate::config::ValidatorConfig {
                enabled: true,
                provider: config.evaluator.provider.clone(),
                model: config.evaluator.model.clone(),
                api_key_env: config.evaluator.api_key_env.clone(),
                max_tokens: config.evaluator.max_tokens,
                temperature: config.evaluator.temperature,
            };
            stores.evaluator = Some(Arc::new(crate::evaluator::CoverageEvaluator::new(eval_config)));
        } else {
            info!("Coverage Evaluator disabled");
        }

        // Load guidance: schema docs from transition rules + LOOPR.md files from disk
        stores.guidance = crate::guidance::load_guidance(&repo_path);

        stores.session_id = session_id.clone();

        Ok(Self {
            config,
            event_tx,
            stores: Arc::new(stores),
            worktree_manager,
            session_id,
            session_dir,
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
            let Ok(mut works) = self.stores.write_works() else {
                return 0;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, wi) in works.iter_mut() {
                if wi.status == WorkStatus::InProgress {
                    warn!("Recovering orphaned InProgress Work: {}", id);
                    wi.status = WorkStatus::Blocked;
                    wi.updated_at = crate::id::now_millis();
                    if let Some(store_arc) = store_lock
                        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                        && let Err(e) = s.update(wi.clone())
                    {
                        warn!("Failed to persist Work recovery to TaskStore: {}", e);
                    }
                    recovered += 1;
                }
            }
        }

        // Recover Integrating Bundles → Accepted
        {
            let Ok(mut bundles) = self.stores.write_bundles() else {
                return 0;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, bundle) in bundles.iter_mut() {
                if bundle.status == BundleStatus::Integrating {
                    warn!("Recovering orphaned Integrating Bundle: {}", id);
                    bundle.status = BundleStatus::Accepted;
                    bundle.updated_at = crate::id::now_millis();
                    if let Some(store_arc) = store_lock
                        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                        && let Err(e) = s.update(bundle.clone())
                    {
                        warn!("Failed to persist Bundle recovery to TaskStore: {}", e);
                    }
                    recovered += 1;
                }
            }
        }

        // Recover stuck Ticks (Open/Sealing/Validating) → Failed
        {
            let Ok(mut ticks) = self.stores.write_ticks() else {
                return 0;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, tick) in ticks.iter_mut() {
                if tick.status == TickStatus::Open
                    || tick.status == TickStatus::Sealing
                    || tick.status == TickStatus::Validating
                {
                    warn!("Recovering stuck Tick in {:?} state: {}", tick.status, id);
                    tick.status = TickStatus::Failed;
                    tick.updated_at = crate::id::now_millis();
                    if let Some(store_arc) = store_lock
                        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                        && let Err(e) = s.update(tick.clone())
                    {
                        warn!("Failed to persist Tick recovery to TaskStore: {}", e);
                    }
                    recovered += 1;
                }
            }
        }

        // Recover stuck AgentSessions (non-terminal) → Failed
        {
            let Ok(mut sessions) = self.stores.write_agent_sessions() else {
                return 0;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, session) in sessions.iter_mut() {
                if !session.status.is_terminal() {
                    warn!("Recovering stuck AgentSession in {:?} state: {}", session.status, id);
                    session.status = AgentStatus::Failed;
                    session.error_message = Some("Recovered after daemon crash".to_string());
                    session.updated_at = crate::id::now_millis();
                    if let Some(store_arc) = store_lock
                        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                        && let Err(e) = s.update(session.clone())
                    {
                        warn!("Failed to persist AgentSession recovery to TaskStore: {}", e);
                    }
                    recovered += 1;
                }
            }
        }

        // Gap #30: Expire stale locks
        {
            let Ok(mut locks) = self.stores.write_locks() else {
                return 0;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, lock) in locks.iter_mut() {
                if lock.is_active() && lock.is_expired() {
                    warn!("Recovering expired Lock: {} (resource={})", id, lock.resource);
                    lock.expire();
                    if let Some(store_arc) = store_lock
                        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                        && let Err(e) = s.update(lock.clone())
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
    pub fn shared(
        config: Config,
        session_id: String,
        session_dir: std::path::PathBuf,
    ) -> eyre::Result<(Arc<RwLock<Self>>, broadcast::Sender<DaemonEvent>)> {
        debug!("DaemonContext::shared()");
        let (event_tx, _) = broadcast::channel::<DaemonEvent>(256);
        let tx = event_tx.clone();
        let ctx = Self::new(config, event_tx, session_id, session_dir)?;
        Ok((Arc::new(RwLock::new(ctx)), tx))
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentType;
    use crate::config::{InterviewMode, ProjectConfig};
    use crate::test_util::TestDir;

    /// Create a test Config with repo_path pointing to a unique temp directory
    /// so TaskStore doesn't pollute the project or collide between tests.
    fn test_config() -> (TestDir, Config) {
        let dir = TestDir::new("loopr-ctx-test");
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        (dir, config)
    }

    #[test]
    fn test_context_new() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();
        assert_eq!(ctx.config.name, "loopr");
        // Fresh TaskStore has no records, so HashMaps are empty after hydration
        assert!(ctx.stores.plans.read().unwrap().is_empty());
    }

    #[test]
    fn test_context_hydrates_from_taskstore() {
        let (_dir, config) = test_config();
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
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();
        assert_eq!(ctx.stores.plans.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.specs.read().unwrap().len(), 1);
    }

    #[test]
    fn test_context_hydrates_all_record_types() {
        let (_dir, config) = test_config();
        let repo_path = config.project.repo_path.clone();

        // Insert one record of each type directly into TaskStore
        {
            let mut store = Store::open(&repo_path).unwrap();
            store.create(Plan::new("P".into(), "d".into(), "c".into())).unwrap();
            store.create(Spec::new("p1".into(), "S".into(), "d".into())).unwrap();
            store
                .create(Phase::new("s1".into(), "Ph".into(), "d".into(), 1))
                .unwrap();
            store.create(Work::new("ph1".into(), "W".into(), "d".into())).unwrap();
            store
                .create(Bundle::new("w1".into(), None, "branch".into(), vec!["claim".into()]))
                .unwrap();
            store.create(Tick::new(1)).unwrap();
            store
                .create(Learning::new(
                    "src1".into(),
                    crate::domain::learning::LearningScope::Work,
                    "content".into(),
                ))
                .unwrap();
            store
                .create(Lock::new("file.rs".into(), "owner".into(), "coordinator".into()))
                .unwrap();
            store.create(CoordinatorGoal::new("goal".into())).unwrap();
            store
                .create(CoordinatorState::new("goal1".into(), InterviewMode::Interactive))
                .unwrap();
            store
                .create(Proposal::new("title".into(), "desc".into(), "author".into()))
                .unwrap();
            store
                .create(Decision::new("title".into(), "rationale".into(), "decider".into()))
                .unwrap();
            store
                .create(AgentSession::new(AgentType::Implementer, "model".into()))
                .unwrap();
        }

        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();
        assert_eq!(ctx.stores.plans.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.specs.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.phases.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.works.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.bundles.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.ticks.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.learnings.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.locks.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.coordinator_goals.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.coordinator_states.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.proposals.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.decisions.read().unwrap().len(), 1);
        assert_eq!(ctx.stores.agent_sessions.read().unwrap().len(), 1);
    }

    #[test]
    fn test_context_new_creates_taskstore() {
        let (_dir, config) = test_config();
        let repo_path = config.project.repo_path.clone();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (ctx, tx) = DaemonContext::shared(
            config,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();
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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();
        let mut rx = ctx.event_tx.subscribe();
        let event = DaemonEvent::record_created("plan", "p1");
        ctx.event_tx.send(event.clone()).unwrap();
        let received = rx.try_recv().unwrap();
        assert_eq!(received.event, "record.created");
    }

    #[test]
    fn test_context_shared_event_broadcast() {
        let (_dir, config) = test_config();
        let (ctx, tx) = DaemonContext::shared(
            config,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();
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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();
        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 0);
    }

    #[test]
    fn test_recover_stuck_tick_sealing() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

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
