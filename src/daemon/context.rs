use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, RwLock as StdRwLock, RwLockReadGuard, RwLockWriteGuard};

use eyre::{Result, eyre};
use paste::paste;
use taskstore::Store;
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, info, warn};

use tokio::task::JoinHandle;

use crate::agents::{AgentEvent, AgentKind, AgentSession, AgentStatus};
use crate::config::Config;
use crate::config::ToolEntry;
use crate::domain::bundle::{Bundle, BundleStatus};
use crate::domain::chat::ChatHistory;
use crate::domain::coordinator_goal::CoordinatorGoal;
use crate::domain::coordinator_state::CoordinatorState;
use crate::domain::coverage::CoverageReport;
use crate::domain::doc::Doc;
use crate::domain::learning::Learning;
use crate::domain::lock::Lock;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::spec::Spec;
use crate::domain::tick::{Tick, TickStatus};
use crate::domain::validation::ValidationReport;
use crate::domain::work::{Work, WorkStatus};
use crate::guidance::AgentGuidance;
use crate::ipc::protocol::{
    DaemonEvent, REASON_HANDLE_FINISHED, REASON_HOLDER_TERMINAL, REASON_HOLDER_WORK_DONE, REASON_LOCK_EXPIRED,
    REASON_MISSING_HANDLE, REASON_STALE_WORKTREE,
};
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
    /// Unified doc storage (new architecture). Coexists with plans/specs/phases/works during migration.
    pub docs: StdRwLock<HashMap<String, Doc>>,
    pub bundles: StdRwLock<HashMap<String, Bundle>>,
    pub ticks: StdRwLock<HashMap<String, Tick>>,
    pub learnings: StdRwLock<HashMap<String, Learning>>,
    pub locks: StdRwLock<HashMap<String, Lock>>,
    pub coordinator_goals: StdRwLock<HashMap<String, CoordinatorGoal>>,
    pub coordinator_states: StdRwLock<HashMap<String, CoordinatorState>>,
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
    /// FSM interpreter for transition validation. Immutable after startup.
    pub fsm: Arc<crate::fsm::runtime::FsmInterpreter>,
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
    /// Degraded mode: set when a catastrophic reconciliation fracture is detected.
    /// Blocks new Tick creation and Implementer spawning until cleared via system.clear_degraded.
    /// Not persisted - re-detected on restart via Integrator git audit.
    pub degraded: AtomicBool,
    /// Reconciliation health stats from the last sweep (updated by run_reconciler).
    pub reconciliation_last_sweep_at: AtomicU64,
    pub reconciliation_checked: AtomicU64,
    pub reconciliation_fixed: AtomicU64,
    pub reconciliation_catastrophic: AtomicU64,
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
        docs: Doc,
        bundles: Bundle,
        ticks: Tick,
        learnings: Learning,
        locks: Lock,
        coordinator_goals: CoordinatorGoal,
        coordinator_states: CoordinatorState,
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

    /// Path to the reconciliation log for this daemon session.
    pub fn reconciliation_log_path(&self) -> Option<std::path::PathBuf> {
        self.session_dir.as_ref().map(|d| d.join("reconciliation.log"))
    }

    /// Append a line to the reconciliation log (best-effort, no error propagation).
    /// Format: `[timestamp LEVEL] collection:id from->to reason`
    pub fn append_reconciliation_log(
        &self,
        level: &str,
        collection: &str,
        id: &str,
        from: &str,
        to: &str,
        reason: &str,
    ) {
        let Some(path) = self.reconciliation_log_path() else {
            return;
        };
        let ts = crate::id::now_millis() as u64;
        let line = format!("[{ts} {level}] {collection}:{id} {from}->{to} {reason}\n");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// Update reconciliation health stats after a sweep completes.
    pub fn update_reconciliation_stats(&self, checked: u64, fixed: u64, catastrophic: u64) {
        self.reconciliation_last_sweep_at
            .store(crate::id::now_millis() as u64, Ordering::Relaxed);
        self.reconciliation_checked.store(checked, Ordering::Relaxed);
        self.reconciliation_fixed.store(fixed, Ordering::Relaxed);
        self.reconciliation_catastrophic.store(catastrophic, Ordering::Relaxed);
    }
}

impl Stores {
    pub fn new() -> Self {
        Self {
            plans: StdRwLock::new(HashMap::new()),
            specs: StdRwLock::new(HashMap::new()),
            phases: StdRwLock::new(HashMap::new()),
            works: StdRwLock::new(HashMap::new()),
            docs: StdRwLock::new(HashMap::new()),
            bundles: StdRwLock::new(HashMap::new()),
            ticks: StdRwLock::new(HashMap::new()),
            learnings: StdRwLock::new(HashMap::new()),
            locks: StdRwLock::new(HashMap::new()),
            coordinator_goals: StdRwLock::new(HashMap::new()),
            coordinator_states: StdRwLock::new(HashMap::new()),
            agent_sessions: StdRwLock::new(HashMap::new()),
            coverage_reports: StdRwLock::new(HashMap::new()),
            runtime_tools: StdRwLock::new(HashMap::new()),
            store: None,
            validator: None,
            evaluator: None,
            tool_runner: StdRwLock::new(Arc::new(ToolRunner::new(&[]))),
            tool_executor: StdRwLock::new(Arc::new(ToolExecutor::standard(&[]))),
            config: Config::default(),
            fsm: Arc::new(
                crate::fsm::runtime::FsmInterpreter::embedded().expect("embedded FSM definitions must be valid"),
            ),
            agent_handles: StdMutex::new(HashMap::new()),
            agent_events: StdRwLock::new(HashMap::new()),
            git_lock: StdMutex::new(()),
            shutting_down: AtomicBool::new(false),
            guidance: AgentGuidance::schema_only(),
            session_dir: None,
            chat_sessions: StdRwLock::new(HashMap::new()),
            session_id: String::new(),
            degraded: AtomicBool::new(false),
            reconciliation_last_sweep_at: AtomicU64::new(0),
            reconciliation_checked: AtomicU64::new(0),
            reconciliation_fixed: AtomicU64::new(0),
            reconciliation_catastrophic: AtomicU64::new(0),
        }
    }
}

/// Record an agent event in the per-session ring buffer.
pub fn record_agent_event(stores: &Stores, session_id: &str, event: &AgentEvent) {
    let Ok(mut events) = stores.write_agent_events() else {
        tracing::error!("agent_events lock poisoned, dropping event for session {session_id}");
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
    pub fsm: Arc<crate::fsm::runtime::FsmInterpreter>,
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
        store.rebuild_indexes::<Doc>()?;
        store.rebuild_indexes::<Bundle>()?;
        store.rebuild_indexes::<Tick>()?;
        store.rebuild_indexes::<Learning>()?;
        store.rebuild_indexes::<Lock>()?;
        store.rebuild_indexes::<CoordinatorGoal>()?;
        store.rebuild_indexes::<CoordinatorState>()?;
        store.rebuild_indexes::<ValidationReport>()?;
        store.rebuild_indexes::<CoverageReport>()?;
        store.rebuild_indexes::<AgentSession>()?;

        info!("TaskStore opened at {}", repo_path.display());

        // Layer 0: Inject Loopr artifacts into .git/info/exclude so they are
        // invisible to all git operations (worktrees inherit automatically).
        if let Err(e) = crate::worktree::manager::ensure_loopr_excludes(&repo_path) {
            warn!("Failed to inject .git/info/exclude: {}", e);
        }

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
            for doc in store.list::<Doc>(&[])? {
                stores.write_docs()?.insert(doc.id.clone(), doc);
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
                + stores.read_docs()?.len()
                + stores.read_bundles()?.len()
                + stores.read_ticks()?.len()
                + stores.read_learnings()?.len()
                + stores.read_locks()?.len()
                + stores.read_coordinator_goals()?.len()
                + stores.read_coordinator_states()?.len()
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
                config.validator.provider, config.validator.llm.model
            );
            stores.validator = Some(Arc::new(DocValidator::new(config.validator.clone())));
        } else {
            info!("Doc Validator disabled");
        }

        // Create CoverageEvaluator if enabled in config
        if config.evaluator.enabled {
            info!(
                "Coverage Evaluator enabled: provider={}, model={}",
                config.evaluator.provider, config.evaluator.llm.model
            );
            let eval_config = crate::config::ValidatorConfig {
                llm: config.evaluator.llm.clone(),
                enabled: true,
                provider: config.evaluator.provider.clone(),
                prompts: crate::config::ValidatorPrompts::default(),
            };
            stores.evaluator = Some(Arc::new(crate::evaluator::CoverageEvaluator::new(eval_config)));
        } else {
            info!("Coverage Evaluator disabled");
        }

        // Load guidance: schema docs from transition rules + LOOPR.md files from disk
        stores.guidance = crate::guidance::load_guidance(&repo_path);

        stores.session_id = session_id.clone();

        let fsm = Arc::new(crate::fsm::runtime::FsmInterpreter::embedded()?);
        stores.fsm = fsm.clone();

        Ok(Self {
            config,
            event_tx,
            stores: Arc::new(stores),
            worktree_manager,
            fsm,
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
                if wi.status() == WorkStatus::InProgress {
                    warn!("Recovering orphaned InProgress Work: {}", id);
                    wi.force_status(WorkStatus::Blocked);
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
                if bundle.status() == BundleStatus::Integrating {
                    warn!("Recovering orphaned Integrating Bundle: {}", id);
                    bundle.force_status(BundleStatus::Accepted);
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
                if tick.status() == TickStatus::Open
                    || tick.status() == TickStatus::Sealing
                    || tick.status() == TickStatus::Validating
                {
                    warn!("Recovering stuck Tick in {:?} state: {}", tick.status(), id);
                    tick.force_status(TickStatus::Failed);
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
                if !session.status().is_terminal() {
                    warn!("Recovering stuck AgentSession in {:?} state: {}", session.status(), id);
                    session.force_status(AgentStatus::Failed);
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

    /// Runtime reconciliation sweep. Detects and recovers from state fractures where
    /// DB state (TaskStore) has diverged from physical state (process handles, worktrees).
    ///
    /// Supersedes `recover_orphaned_records()` for runtime use. At startup, `agent_handles`
    /// is empty so all non-terminal sessions are correctly failed (same result as the old
    /// conservative reset). At runtime, sessions with live handles are left untouched.
    ///
    /// Returns the number of records fixed.
    pub fn reconcile(&self) -> usize {
        let mut fixed = 0usize;
        let mut checked = 0usize;

        // Pre-compute handle state snapshot to avoid holding mutex across lock acquisitions.
        let handles_state: HashMap<String, bool> = {
            match self.stores.lock_agent_handles() {
                Ok(handles) => handles.iter().map(|(id, h)| (id.clone(), h.is_finished())).collect(),
                Err(e) => {
                    warn!("Reconciliation: cannot read agent_handles: {}", e);
                    HashMap::new()
                }
            }
        };

        // Pre-compute work_ids of sessions with live handles (neither missing nor finished).
        let active_work_ids: HashSet<String> = {
            match self.stores.read_agent_sessions() {
                Ok(sessions) => sessions
                    .values()
                    .filter(|s| !s.status().is_terminal())
                    .filter(|s| handles_state.get(&s.id) == Some(&false))
                    .filter_map(|s| s.work_id.clone())
                    .collect(),
                Err(_) => HashSet::new(),
            }
        };

        // Pre-compute terminal work_ids (Done or Abandoned).
        let terminal_work_ids: HashSet<String> = {
            match self.stores.read_works() {
                Ok(works) => works
                    .values()
                    .filter(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
                    .map(|w| w.id.clone())
                    .collect(),
                Err(_) => HashSet::new(),
            }
        };

        // --- Session vs Handle cross-check ---
        {
            let session_timeout_secs = self.config.agents.implementer.session_timeout_secs.unwrap_or(30);
            let now_ms = crate::id::now_millis();
            let Ok(mut sessions) = self.stores.write_agent_sessions() else {
                warn!("Reconciliation: cannot write agent_sessions, skipping");
                return 0;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, session) in sessions.iter_mut() {
                checked += 1;
                if session.status().is_terminal() {
                    continue;
                }
                let from = format!("{:?}", session.status());
                match handles_state.get(id) {
                    None => {
                        // Starting: allow a grace period before declaring failure
                        if session.status() == AgentStatus::Starting {
                            let age_secs = ((now_ms - session.created_at) / 1000) as u64;
                            if age_secs <= session_timeout_secs {
                                continue; // still within startup grace period
                            }
                        }
                        warn!(
                            "Reconciliation: Session {} no handle (status={}, age={}s)",
                            id,
                            from,
                            (now_ms - session.created_at) / 1000
                        );
                        session.force_status(AgentStatus::Failed);
                        session.error_message = Some("Reconciliation: no task handle found".to_string());
                        session.updated_at = crate::id::now_millis();
                        if let Some(store_arc) = store_lock
                            && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                            && let Err(e) = s.update(session.clone())
                        {
                            warn!("Reconciliation: failed to persist session {}: {}", id, e);
                        }
                        self.stores.append_reconciliation_log(
                            "WARN",
                            "agent_session",
                            id,
                            &from,
                            "Failed",
                            REASON_MISSING_HANDLE,
                        );
                        let _ = self.event_tx.send(DaemonEvent::reconciled(
                            "agent_session",
                            id,
                            &from,
                            "Failed",
                            REASON_MISSING_HANDLE,
                        ));
                        fixed += 1;
                    }
                    Some(true) => {
                        // Handle exists but is finished - task ended without updating status
                        warn!("Reconciliation: Session {} handle finished but status={}", id, from);
                        session.force_status(AgentStatus::Failed);
                        session.error_message =
                            Some("Reconciliation: task handle finished without status update".to_string());
                        session.updated_at = crate::id::now_millis();
                        if let Some(store_arc) = store_lock
                            && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                            && let Err(e) = s.update(session.clone())
                        {
                            warn!("Reconciliation: failed to persist session {}: {}", id, e);
                        }
                        self.stores.append_reconciliation_log(
                            "WARN",
                            "agent_session",
                            id,
                            &from,
                            "Failed",
                            REASON_HANDLE_FINISHED,
                        );
                        let _ = self.event_tx.send(DaemonEvent::reconciled(
                            "agent_session",
                            id,
                            &from,
                            "Failed",
                            REASON_HANDLE_FINISHED,
                        ));
                        fixed += 1;
                    }
                    Some(false) => {
                        // Handle exists and is running - agent is active, skip
                    }
                }
            }
            // Cleanup: remove handles for terminal sessions (no status change needed)
            drop(sessions);
            if let Ok(mut handles) = self.stores.lock_agent_handles()
                && let Ok(sessions) = self.stores.read_agent_sessions()
            {
                handles.retain(|id, _| sessions.get(id.as_str()).is_none_or(|s| !s.status().is_terminal()));
            }
        }

        // --- Work: InProgress with no active session ---
        // If the work has a Merged bundle, the implementation is complete: advance to Done.
        // Otherwise the work was abandoned mid-flight: move to Blocked for recovery.
        {
            // Collect merged work IDs before acquiring the write lock on works (lock ordering).
            let merged_work_ids: HashSet<String> = self
                .stores
                .read_bundles()
                .map(|bundles| {
                    bundles
                        .values()
                        .filter(|b| b.status() == BundleStatus::Merged)
                        .map(|b| b.work_id.clone())
                        .collect()
                })
                .unwrap_or_default();

            let Ok(mut works) = self.stores.write_works() else {
                warn!("Reconciliation: cannot write works, skipping");
                return fixed;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, wi) in works.iter_mut() {
                checked += 1;
                if wi.status() != WorkStatus::InProgress {
                    continue;
                }
                if active_work_ids.contains(id) {
                    continue; // active agent is running this work
                }
                let from = "InProgress";
                let (to_status, to_str) = if merged_work_ids.contains(id) {
                    warn!(
                        "Reconciliation: Work {} InProgress with Merged bundle, advancing to Done",
                        id
                    );
                    (WorkStatus::Done, "Done")
                } else {
                    warn!("Reconciliation: Work {} InProgress with no active session", id);
                    (WorkStatus::Blocked, "Blocked")
                };
                wi.force_status(to_status);
                wi.updated_at = crate::id::now_millis();
                if let Some(store_arc) = store_lock
                    && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                    && let Err(e) = s.update(wi.clone())
                {
                    warn!("Reconciliation: failed to persist work {}: {}", id, e);
                }
                self.stores
                    .append_reconciliation_log("WARN", "work", id, from, to_str, REASON_MISSING_HANDLE);
                let _ = self
                    .event_tx
                    .send(DaemonEvent::reconciled("work", id, from, to_str, REASON_MISSING_HANDLE));
                fixed += 1;
            }
        }

        // --- Bundle: Integrating → Accepted ---
        {
            let Ok(mut bundles) = self.stores.write_bundles() else {
                warn!("Reconciliation: cannot write bundles, skipping");
                return fixed;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, bundle) in bundles.iter_mut() {
                checked += 1;
                if bundle.status() != BundleStatus::Integrating {
                    continue;
                }
                let from = "Integrating";
                warn!("Reconciliation: Bundle {} stuck in Integrating", id);
                bundle.force_status(BundleStatus::Accepted);
                bundle.updated_at = crate::id::now_millis();
                if let Some(store_arc) = store_lock
                    && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                    && let Err(e) = s.update(bundle.clone())
                {
                    warn!("Reconciliation: failed to persist bundle {}: {}", id, e);
                }
                self.stores
                    .append_reconciliation_log("WARN", "bundle", id, from, "Accepted", REASON_MISSING_HANDLE);
                let _ = self.event_tx.send(DaemonEvent::reconciled(
                    "bundle",
                    id,
                    from,
                    "Accepted",
                    REASON_MISSING_HANDLE,
                ));
                fixed += 1;
            }
        }

        // --- Tick: stuck Open/Sealing/Validating → Failed ---
        {
            let Ok(mut ticks) = self.stores.write_ticks() else {
                warn!("Reconciliation: cannot write ticks, skipping");
                return fixed;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, tick) in ticks.iter_mut() {
                checked += 1;
                let status = tick.status();
                if !matches!(status, TickStatus::Open | TickStatus::Sealing | TickStatus::Validating) {
                    continue;
                }
                let from = format!("{:?}", status);
                warn!("Reconciliation: Tick {} stuck in {:?}", id, status);
                tick.force_status(TickStatus::Failed);
                if let Some(store_arc) = store_lock
                    && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                    && let Err(e) = s.update(tick.clone())
                {
                    warn!("Reconciliation: failed to persist tick {}: {}", id, e);
                }
                self.stores
                    .append_reconciliation_log("WARN", "tick", id, &from, "Failed", REASON_MISSING_HANDLE);
                let _ = self.event_tx.send(DaemonEvent::reconciled(
                    "tick",
                    id,
                    &from,
                    "Failed",
                    REASON_MISSING_HANDLE,
                ));
                fixed += 1;
            }
        }

        // --- Lock: expired TTL + holder-status-aware release ---
        {
            let Ok(mut locks) = self.stores.write_locks() else {
                warn!("Reconciliation: cannot write locks, skipping");
                return fixed;
            };
            let store_lock = self.stores.store.as_ref();
            for (id, lock) in locks.iter_mut() {
                checked += 1;
                if !lock.is_active() {
                    continue;
                }
                // Check expired TTL first
                if lock.is_expired() {
                    let from = "Active";
                    warn!("Reconciliation: Lock {} expired (resource={})", id, lock.resource);
                    lock.expire();
                    if let Some(store_arc) = store_lock
                        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                        && let Err(e) = s.update(lock.clone())
                    {
                        warn!("Reconciliation: failed to persist lock {}: {}", id, e);
                    }
                    self.stores
                        .append_reconciliation_log("WARN", "lock", id, from, "Expired", REASON_LOCK_EXPIRED);
                    let _ = self.event_tx.send(DaemonEvent::reconciled(
                        "lock",
                        id,
                        from,
                        "Expired",
                        REASON_LOCK_EXPIRED,
                    ));
                    fixed += 1;
                    continue;
                }
                // Holder work is Done/Abandoned → release lock
                if terminal_work_ids.contains(&lock.holder_id) {
                    let from = "Active";
                    warn!(
                        "Reconciliation: Lock {} released (holder work {} is terminal)",
                        id, lock.holder_id
                    );
                    lock.release();
                    if let Some(store_arc) = store_lock
                        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                        && let Err(e) = s.update(lock.clone())
                    {
                        warn!("Reconciliation: failed to persist lock {}: {}", id, e);
                    }
                    self.stores.append_reconciliation_log(
                        "WARN",
                        "lock",
                        id,
                        from,
                        "Released",
                        REASON_HOLDER_WORK_DONE,
                    );
                    let _ = self.event_tx.send(DaemonEvent::reconciled(
                        "lock",
                        id,
                        from,
                        "Released",
                        REASON_HOLDER_WORK_DONE,
                    ));
                    fixed += 1;
                    continue;
                }
                // Holder agent has no active session → release lock
                if !active_work_ids.contains(&lock.holder_id) {
                    // Double-check: is the holder work even in a working state?
                    let holder_is_working = {
                        self.stores
                            .read_works()
                            .ok()
                            .and_then(|ws| ws.get(lock.holder_id.as_str()).map(|w| w.status()))
                            .is_some_and(|s| {
                                matches!(s, WorkStatus::InProgress | WorkStatus::InReview | WorkStatus::Ready)
                            })
                    };
                    if holder_is_working {
                        // Work is in an active state but has no running session - could be transient
                        // during agent startup. Let the Work reconciliation handle it.
                        continue;
                    }
                    let from = "Active";
                    warn!(
                        "Reconciliation: Lock {} released (holder {} has no active session)",
                        id, lock.holder_id
                    );
                    lock.release();
                    if let Some(store_arc) = store_lock
                        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                        && let Err(e) = s.update(lock.clone())
                    {
                        warn!("Reconciliation: failed to persist lock {}: {}", id, e);
                    }
                    self.stores
                        .append_reconciliation_log("WARN", "lock", id, from, "Released", REASON_HOLDER_TERMINAL);
                    let _ = self.event_tx.send(DaemonEvent::reconciled(
                        "lock",
                        id,
                        from,
                        "Released",
                        REASON_HOLDER_TERMINAL,
                    ));
                    fixed += 1;
                }
            }
        }

        // --- Stale worktree cleanup ---
        // Work is Done/Abandoned AND worktree exists AND no active agent session for this work.
        {
            let Ok(works) = self.stores.read_works() else {
                warn!("Reconciliation: cannot read works for worktree check");
                return fixed;
            };
            let done_work_ids: Vec<String> = works
                .values()
                .filter(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
                .filter(|w| !active_work_ids.contains(&w.id))
                .map(|w| w.id.clone())
                .collect();
            drop(works);

            for work_id in done_work_ids {
                checked += 1;
                if self.worktree_manager.exists(&work_id) {
                    warn!("Reconciliation: stale worktree for Done/Abandoned Work {}", work_id);
                    if let Err(e) = self.worktree_manager.cleanup(&work_id) {
                        warn!("Reconciliation: failed to cleanup worktree for {}: {}", work_id, e);
                    } else {
                        self.stores.append_reconciliation_log(
                            "WARN",
                            "work",
                            &work_id,
                            "Done/Abandoned",
                            "NoWorktree",
                            REASON_STALE_WORKTREE,
                        );
                        let _ = self.event_tx.send(DaemonEvent::reconciled(
                            "work",
                            &work_id,
                            "Done/Abandoned",
                            "NoWorktree",
                            REASON_STALE_WORKTREE,
                        ));
                        fixed += 1;
                    }
                }
            }
        }

        info!(
            "Reconciliation sweep completed: checked={} fixed={} catastrophic=0",
            checked, fixed
        );
        self.stores.update_reconciliation_stats(checked as u64, fixed as u64, 0);

        fixed
    }

    /// Returns bundle IDs of Triaged bundles with no non-terminal reviewer session.
    /// Used by the reconciler to identify bundles that need a reviewer spawned.
    pub fn triaged_bundles_needing_reviewer(&self) -> Vec<String> {
        let active_reviewer_bundle_ids: HashSet<String> = self
            .stores
            .read_agent_sessions()
            .map(|sessions| {
                sessions
                    .values()
                    .filter(|s| s.agent_type == AgentKind::Reviewer && !s.status().is_terminal())
                    .filter_map(|s| s.bundle_id.clone())
                    .collect()
            })
            .unwrap_or_default();

        self.stores
            .read_bundles()
            .map(|bundles| {
                bundles
                    .values()
                    .filter(|b| b.status() == BundleStatus::Triaged)
                    .filter(|b| !active_reviewer_bundle_ids.contains(&b.id))
                    .map(|b| b.id.clone())
                    .collect()
            })
            .unwrap_or_default()
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
    use crate::agents::AgentKind;
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
            let plan = Plan::new("Hydration Test".into(), "Criteria".into());
            store.create(plan).unwrap();
            let spec = Spec::new("plan-1".into(), "Spec".into());
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
            store.create(Plan::new("P".into(), "c".into())).unwrap();
            store.create(Spec::new("p1".into(), "S".into())).unwrap();
            store.create(Phase::new("s1".into(), "Ph".into())).unwrap();
            store.create(Work::new("ph1".into(), "W".into())).unwrap();
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
                .create(AgentSession::new(AgentKind::Implementer, "model".into()))
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
        let plan = Plan::new("Test Plan".into(), "Criteria".into());
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
        let plan = Plan::new("Test".into(), "Criteria".into());
        let id = plan.id.clone();
        stores.plans.write().unwrap().insert(id.clone(), plan);
        let plans = stores.plans.read().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[&id].title, "Test");
    }

    #[test]
    fn test_stores_spec_insert_and_read() {
        let stores = Stores::new();
        let spec = Spec::new("plan-1".into(), "Test Spec".into());
        let id = spec.id.clone();
        stores.specs.write().unwrap().insert(id.clone(), spec);
        let specs = stores.specs.read().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[&id].title, "Test Spec");
    }

    #[test]
    fn test_stores_phase_insert_and_read() {
        let stores = Stores::new();
        let phase = Phase::new("spec-1".into(), "Test Phase".into());
        let id = phase.id.clone();
        stores.phases.write().unwrap().insert(id.clone(), phase);
        let phases = stores.phases.read().unwrap();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[&id].title, "Test Phase");
    }

    #[test]
    fn test_stores_work_insert_and_read() {
        let stores = Stores::new();
        let wi = Work::new("phase-1".into(), "Test WI".into());
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
        let mut wi = Work::new("phase-1".into(), "Orphaned WI".into());
        wi.force_status(WorkStatus::InProgress);
        let wi_id = wi.id.clone();
        ctx.stores.works.write().unwrap().insert(wi_id.clone(), wi);

        // Insert a Work in Draft state (not orphaned)
        let wi2 = Work::new("phase-1".into(), "Normal WI".into());
        let wi2_id = wi2.id.clone();
        ctx.stores.works.write().unwrap().insert(wi2_id.clone(), wi2);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 1);

        let works = ctx.stores.works.read().unwrap();
        assert_eq!(works[&wi_id].status(), WorkStatus::Blocked);
        assert_eq!(works[&wi2_id].status(), WorkStatus::Draft);
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
        bundle.force_status(BundleStatus::Integrating);
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
        assert_eq!(bundles[&b_id].status(), BundleStatus::Accepted);
        assert_eq!(bundles[&b2_id].status(), BundleStatus::Proposed);
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

        let mut wi = Work::new("phase-1".into(), "Orphaned WI".into());
        wi.force_status(WorkStatus::InProgress);
        ctx.stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut bundle = Bundle::new(
            "wi-1".into(),
            Some("tick-1".into()),
            "feature/orphaned".into(),
            vec!["claims".into()],
        );
        bundle.force_status(BundleStatus::Integrating);
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
        let wi = Work::new("phase-1".into(), "Normal WI".into());
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
        tick.force_status(TickStatus::Sealing);
        let tick_id = tick.id.clone();
        ctx.stores.ticks.write().unwrap().insert(tick_id.clone(), tick);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 1);

        let ticks = ctx.stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status(), TickStatus::Failed);
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
        tick.force_status(TickStatus::Validating);
        let tick_id = tick.id.clone();
        ctx.stores.ticks.write().unwrap().insert(tick_id.clone(), tick);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 1);

        let ticks = ctx.stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status(), TickStatus::Failed);
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
        assert_eq!(ticks[&tick_id].status(), TickStatus::Failed);
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
        let mut wi = Work::new("phase-1".into(), "Orphaned WI".into());
        wi.force_status(WorkStatus::InProgress);
        ctx.stores.works.write().unwrap().insert(wi.id.clone(), wi);

        // Stuck Sealing Tick
        let mut tick = Tick::new(1);
        tick.force_status(TickStatus::Sealing);
        ctx.stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        // Orphaned Integrating Bundle
        let mut bundle = Bundle::new(
            "wi-1".into(),
            Some("tick-1".into()),
            "feature/orphaned".into(),
            vec!["claims".into()],
        );
        bundle.force_status(BundleStatus::Integrating);
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

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".into());
        session.force_status(AgentStatus::Running);
        let session_id = session.id.clone();
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let recovered = ctx.recover_orphaned_records();
        assert!(recovered >= 1);

        let sessions = ctx.stores.agent_sessions.read().unwrap();
        assert_eq!(sessions[&session_id].status(), AgentStatus::Failed);
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

        let mut session = AgentSession::new(AgentKind::Coordinator, "test-model".into());
        session.force_status(AgentStatus::Running);
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
        assert_eq!(sessions[&session_id].status(), AgentStatus::Failed);
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

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".into());
        session.force_status(AgentStatus::Completed);
        let session_id = session.id.clone();
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let recovered = ctx.recover_orphaned_records();
        assert_eq!(recovered, 0);

        let sessions = ctx.stores.agent_sessions.read().unwrap();
        assert_eq!(sessions[&session_id].status(), AgentStatus::Completed);
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

    #[test]
    fn test_stores_degraded_initializes_false() {
        let stores = Stores::new();
        assert!(!stores.degraded.load(Ordering::Relaxed));
    }

    #[test]
    fn test_stores_reconciliation_stats_initialize_zero() {
        let stores = Stores::new();
        assert_eq!(stores.reconciliation_last_sweep_at.load(Ordering::Relaxed), 0);
        assert_eq!(stores.reconciliation_checked.load(Ordering::Relaxed), 0);
        assert_eq!(stores.reconciliation_fixed.load(Ordering::Relaxed), 0);
        assert_eq!(stores.reconciliation_catastrophic.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_stores_update_reconciliation_stats() {
        let stores = Stores::new();
        stores.update_reconciliation_stats(42, 3, 0);
        assert!(stores.reconciliation_last_sweep_at.load(Ordering::Relaxed) > 0);
        assert_eq!(stores.reconciliation_checked.load(Ordering::Relaxed), 42);
        assert_eq!(stores.reconciliation_fixed.load(Ordering::Relaxed), 3);
        assert_eq!(stores.reconciliation_catastrophic.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_stores_reconciliation_log_path() {
        let mut stores = Stores::new();
        // No session_dir set: returns None
        assert!(stores.reconciliation_log_path().is_none());
        // With session_dir set: returns path in that dir
        stores.session_dir = Some(std::path::PathBuf::from("/tmp/loopr-test-session"));
        let path = stores.reconciliation_log_path().unwrap();
        assert_eq!(path.file_name().unwrap(), "reconciliation.log");
        assert!(path.to_str().unwrap().contains("loopr-test-session"));
    }

    // --- reconcile() tests ---

    #[test]
    fn test_reconcile_inprogress_work_no_active_session() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // InProgress work with no session → should be reset to Blocked
        let mut wi = Work::new("phase-1".into(), "Orphaned WI".into());
        wi.force_status(WorkStatus::InProgress);
        let wi_id = wi.id.clone();
        ctx.stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let fixed = ctx.reconcile();
        assert_eq!(fixed, 1);
        let works = ctx.stores.works.read().unwrap();
        assert_eq!(works[&wi_id].status(), WorkStatus::Blocked);
    }

    #[test]
    fn test_reconcile_inprogress_with_merged_bundle_advances_to_done() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // InProgress work with no session but a Merged bundle → should advance to Done
        let mut wi = Work::new("phase-1".into(), "Merged WI".into());
        wi.force_status(WorkStatus::InProgress);
        let wi_id = wi.id.clone();
        ctx.stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let mut bundle = Bundle::new(wi_id.clone(), None, "agent/merged-wi".into(), vec![]);
        bundle.force_status(BundleStatus::Merged);
        ctx.stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let fixed = ctx.reconcile();
        assert_eq!(fixed, 1);
        let works = ctx.stores.works.read().unwrap();
        assert_eq!(works[&wi_id].status(), WorkStatus::Done);
    }

    #[test]
    fn test_reconcile_inprogress_without_merged_bundle_stays_blocked() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // InProgress work with a Rejected bundle and no Merged bundle → Blocked
        let mut wi = Work::new("phase-1".into(), "Rejected WI".into());
        wi.force_status(WorkStatus::InProgress);
        let wi_id = wi.id.clone();
        ctx.stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let mut bundle = Bundle::new(wi_id.clone(), None, "agent/rejected-wi".into(), vec![]);
        bundle.force_status(BundleStatus::Rejected);
        ctx.stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let fixed = ctx.reconcile();
        assert_eq!(fixed, 1);
        let works = ctx.stores.works.read().unwrap();
        assert_eq!(works[&wi_id].status(), WorkStatus::Blocked);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_reconcile_inprogress_work_with_active_session_is_skipped() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        let mut wi = Work::new("phase-1".into(), "Active WI".into());
        wi.force_status(WorkStatus::InProgress);
        let wi_id = wi.id.clone();
        ctx.stores.works.write().unwrap().insert(wi_id.clone(), wi);

        // Spawn a task that never completes to simulate an active agent handle
        let handle = tokio::spawn(std::future::pending::<()>());
        assert!(!handle.is_finished(), "handle should not be finished");

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".to_string());
        session.work_id = Some(wi_id.clone());
        session.force_status(AgentStatus::Running);
        let session_id = session.id.clone();
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);
        ctx.stores.agent_handles.lock().unwrap().insert(session_id, handle);

        let fixed = ctx.reconcile();
        // Work should NOT be reset since it has an active session with a live handle
        assert_eq!(fixed, 0);
        let works = ctx.stores.works.read().unwrap();
        assert_eq!(works[&wi_id].status(), WorkStatus::InProgress);
    }

    #[test]
    fn test_reconcile_session_no_handle() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".to_string());
        session.force_status(AgentStatus::Running);
        let session_id = session.id.clone();
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);
        // No handle inserted

        let fixed = ctx.reconcile();
        assert_eq!(fixed, 1);
        let sessions = ctx.stores.agent_sessions.read().unwrap();
        assert_eq!(sessions[&session_id].status(), AgentStatus::Failed);
    }

    #[test]
    fn test_reconcile_integrating_bundle() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        let mut bundle = Bundle::new("wi-1".into(), None, "feature/test".into(), vec![]);
        bundle.force_status(BundleStatus::Integrating);
        let b_id = bundle.id.clone();
        ctx.stores.bundles.write().unwrap().insert(b_id.clone(), bundle);

        let fixed = ctx.reconcile();
        assert_eq!(fixed, 1);
        let bundles = ctx.stores.bundles.read().unwrap();
        assert_eq!(bundles[&b_id].status(), BundleStatus::Accepted);
    }

    #[test]
    fn test_reconcile_lock_released_when_holder_work_done() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        let mut wi = Work::new("phase-1".into(), "Done WI".into());
        wi.force_status(WorkStatus::Done);
        let wi_id = wi.id.clone();
        ctx.stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let lock = Lock::new("src/lib.rs".into(), wi_id.clone(), "coordinator".into());
        let lock_id = lock.id.clone();
        ctx.stores.locks.write().unwrap().insert(lock_id.clone(), lock);

        let fixed = ctx.reconcile();
        assert_eq!(fixed, 1);
        let locks = ctx.stores.locks.read().unwrap();
        assert!(!locks[&lock_id].is_active());
    }

    #[test]
    fn test_reconcile_no_action_needed() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // Draft work and proposed bundle — both in safe states
        let wi = Work::new("phase-1".into(), "Normal WI".into());
        ctx.stores.works.write().unwrap().insert(wi.id.clone(), wi);
        let bundle = Bundle::new("wi-1".into(), None, "feature/ok".into(), vec![]);
        ctx.stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let fixed = ctx.reconcile();
        assert_eq!(fixed, 0);
    }

    /// Regression: worker-spawned sessions previously bypassed handle registration,
    /// causing the reconciler to kill the session (Running, no handle -> Failed) and
    /// then reset the work (InProgress, no live session -> Blocked) in a single sweep.
    ///
    /// The reconciler computes active_work_ids before the session sweep, so a Running
    /// session with no registered handle is excluded from active_work_ids immediately.
    /// This means both the session→Failed and work→Blocked corrections happen in one
    /// reconcile() call — not two separate sweeps.
    ///
    /// After the fix, the worker registers a handle, so the session and work survive.
    /// This test documents the one-sweep cascade that the fix prevents.
    #[tokio::test]
    async fn test_reconcile_inprogress_work_session_no_handle_resets_in_two_sweeps() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // InProgress work + Running session claiming it, but NO handle registered.
        // This is the pre-fix state for worker-spawned implementers.
        let mut wi = Work::new("phase-1".into(), "Worker WI".into());
        wi.force_status(WorkStatus::InProgress);
        let wi_id = wi.id.clone();
        ctx.stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".to_string());
        session.work_id = Some(wi_id.clone());
        session.force_status(AgentStatus::Running);
        let session_id = session.id.clone();
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);
        // Deliberately NO handle inserted — this is the bug scenario.

        // Single sweep: active_work_ids is computed before the session sweep, so the
        // no-handle Running session is excluded immediately. Both the session→Failed
        // correction AND the work→Blocked correction happen in the same reconcile() call.
        let fixed = ctx.reconcile();
        assert!(
            fixed >= 1,
            "single sweep should fix both the orphaned session and the orphaned work"
        );
        let session_status = ctx.stores.agent_sessions.read().unwrap()[&session_id].status();
        assert_eq!(
            session_status,
            AgentStatus::Failed,
            "session should be Failed after one sweep"
        );
        let work_status = ctx.stores.works.read().unwrap()[&wi_id].status();
        assert_eq!(
            work_status,
            WorkStatus::Blocked,
            "work should be Blocked after one sweep"
        );
    }

    /// Positive case: worker registers handle -> session stays Running, work stays InProgress.
    /// This is the post-fix expected behavior verified directly.
    #[tokio::test]
    async fn test_reconcile_inprogress_work_with_registered_handle_survives() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(
            config,
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        let mut wi = Work::new("phase-1".into(), "Worker WI".into());
        wi.force_status(WorkStatus::InProgress);
        let wi_id = wi.id.clone();
        ctx.stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".to_string());
        session.work_id = Some(wi_id.clone());
        session.force_status(AgentStatus::Running);
        let session_id = session.id.clone();
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Register a live handle (never-completing task) — this is what the fix adds.
        let handle = tokio::spawn(std::future::pending::<()>());
        ctx.stores
            .agent_handles
            .lock()
            .unwrap()
            .insert(session_id.clone(), handle);

        let fixed = ctx.reconcile();
        assert_eq!(fixed, 0, "no fixes should be needed with a registered handle");
        assert_eq!(
            ctx.stores.works.read().unwrap()[&wi_id].status(),
            WorkStatus::InProgress,
            "work must stay InProgress while session has a live handle"
        );
        assert_eq!(
            ctx.stores.agent_sessions.read().unwrap()[&session_id].status(),
            AgentStatus::Running,
            "session must stay Running with a live handle"
        );
    }

    #[test]
    fn test_triaged_bundles_needing_reviewer_returns_unserviced() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx, "test".into(), std::path::PathBuf::from("/tmp")).unwrap();

        // Create a Triaged bundle with no reviewer session
        let mut bundle = Bundle::new("wi-1".into(), None, "branch-a".into(), vec![]);
        bundle.force_status(crate::domain::bundle::BundleStatus::Triaged);
        let bundle_id = bundle.id.clone();
        ctx.stores.bundles.write().unwrap().insert(bundle_id.clone(), bundle);

        let needing = ctx.triaged_bundles_needing_reviewer();
        assert_eq!(needing, vec![bundle_id]);
    }

    #[test]
    fn test_triaged_bundles_needing_reviewer_excludes_with_active_session() {
        let (_dir, config) = test_config();
        let (tx, _rx) = broadcast::channel(16);
        let ctx = DaemonContext::new(config, tx, "test".into(), std::path::PathBuf::from("/tmp")).unwrap();

        // Create a Triaged bundle
        let mut bundle = Bundle::new("wi-1".into(), None, "branch-a".into(), vec![]);
        bundle.force_status(crate::domain::bundle::BundleStatus::Triaged);
        let bundle_id = bundle.id.clone();
        ctx.stores.bundles.write().unwrap().insert(bundle_id.clone(), bundle);

        // Create an active reviewer session for this bundle
        let mut session = AgentSession::new(AgentKind::Reviewer, "test-model".into());
        session.bundle_id = Some(bundle_id.clone());
        session.force_status(AgentStatus::Running);
        ctx.stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let needing = ctx.triaged_bundles_needing_reviewer();
        assert!(needing.is_empty(), "bundle with active reviewer should not be returned");
    }
}
