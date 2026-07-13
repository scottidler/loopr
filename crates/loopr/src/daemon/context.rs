//! `DaemonContext`: shared state for the daemon run.
//!
//! Held in an `Arc` by the accept loop, each connection-handler task, and
//! the signal-watcher task. Values are set once at startup and read-only
//! thereafter; the only mutable cell is `shutting_down`.

use std::collections::HashMap;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore, broadcast};
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use agents::{
    CheckRunner, Deps, DirectorConfig, DirectorStatusSnapshot, ImplementerConfig, ImplementerError,
    ProductionCheckRunner, RealTools, ReviewerConfig, ReviewerDeps, ReviewerError, render_review_feedback,
    run_implementer, run_reviewer,
};
use context::{InlineContextBuilder, StateSummary};
use domain::{Bundle, BundleId, BundleStatus, FailureReason, PlanId, Role, Verdict, Work, WorkId, WorkStatus};
use futures_util::FutureExt;
// Stage 8 used to consume `BundleUpdateError` here; Director Phase 3
// shifts that match into the `WorkSpawner::accept_bundle` path which
// matches `StoreError::Stale` directly. The import is kept available for
// callers/sinks even though it's no longer named in this module.
use integrator::IntegratorConfig;
use ipc::DaemonEvent;
use llm::LlmClient;
use store::Store;
use telemetry::digest::process::ProcessSnapshot;
use telemetry::{ProcessId, SessionId};
use tools::{BashDenylist, LaneRouter, SandboxMode, ToolContext};
use worktree::{AttemptCleanupPolicy, Worktree};

/// Exponential backoff schedule for Integrator retries on transient
/// errors (`IntegrationError::Update(Stale)` or `Store(_)`). Five
/// attempts total; Integrator doc mandates a circuit-breaker cap.
/// Selected to cover typical OCC-race windows without starving
/// shutdown signals.
pub const INTEGRATOR_BACKOFF: &[Duration] = &[
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(5),
];

/// Capacity of the daemon's event broadcast channel. Sized for a slow
/// `loopr watch` terminal (Phase 17 of
/// `docs/design/2026-07-11-verified-swarm.md`): a human-driven consumer
/// that pauses to read a burst must not force the daemon's broadcast ring
/// to drop events (and trip the gap marker) under normal activity. When a
/// consumer genuinely falls this far behind, the subscribe path still
/// surfaces a typed gap marker rather than silently losing events. Bumped
/// from the v4 value of 64.
pub const EVENTS_CAPACITY: usize = 1024;

/// Extract a human-readable message from a caught panic payload. The
/// standard library boxes panic payloads as `&str` (the common
/// `panic!("...")` / `unwrap` case) or `String`; anything else
/// (`panic_any`) is opaque. Used by the daemon's `catch_unwind` panic
/// posture to log + record what failed without re-panicking.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Phase 2 sidecar-map presence guard. Inserts `key` into `map` on
/// construction; removes on `Drop` so `panic!` or normal exit both clean
/// up. Owns an `Arc` clone of the map (no borrow lifetime) so it can be
/// moved into a `tokio::spawn` body that requires `'static`.
///
/// Used by the three task-body wrappers (`spawn_implementer_for_work`,
/// `spawn_reviewer_for_bundle`, `spawn_integrator_for_bundle`) to
/// expose live-task IDs through the `WorkSpawner::list_running_*_ids`
/// surface. The `Drop` write uses blocking `write()`; the lock is held
/// for one `HashMap::remove` call and never crosses an `.await`.
pub struct ScopedIdGuard<K: Hash + Eq + Clone> {
    map: Arc<StdRwLock<HashMap<K, ()>>>,
    key: K,
}

impl<K: Hash + Eq + Clone> ScopedIdGuard<K> {
    /// Insert `key` into `map`, returning a guard that removes it on `Drop`.
    pub fn new(map: Arc<StdRwLock<HashMap<K, ()>>>, key: K) -> Self {
        if let Ok(mut m) = map.write() {
            m.insert(key.clone(), ());
        }
        Self { map, key }
    }
}

impl<K: Hash + Eq + Clone> Drop for ScopedIdGuard<K> {
    fn drop(&mut self) {
        // Blocking write: the lock is contended only by other inserts /
        // removes from sibling task bodies, each of which holds it for
        // microseconds. Poison (writer panicked while holding the lock)
        // degrades to a leaked entry; that's acceptable - the worst case
        // is the reconcile sweep skipping a phantom-live ID once.
        if let Ok(mut m) = self.map.write() {
            m.remove(&self.key);
        }
    }
}

pub struct DaemonContext<L: LlmClient + Send + Sync + 'static> {
    pub target: PathBuf,
    pub session_id: SessionId,
    pub target_slug: String,
    pub process_id: ProcessId,
    pub started_at: chrono::DateTime<chrono::Local>,
    pub pid: u32,
    /// Broadcast bus for `DaemonEvent`s. Stage 4 defines the channel but
    /// never fires an event. Stage 5+ fires on record transitions.
    pub events: broadcast::Sender<DaemonEvent>,
    /// Set to `true` by the signal-watcher task or by an in-process
    /// shutdown request. `accept_loop` and every `handle_client` read it
    /// to decide whether to exit.
    pub shutting_down: Arc<AtomicBool>,
    /// Async-friendly wakeup. A single `tokio::signal`-driven task awaits
    /// SIGTERM/SIGINT; on signal it sets `shutting_down = true` and calls
    /// `shutdown_notify.notify_waiters()`, which wakes every consumer
    /// that called `shutdown_notify.notified().await`. This avoids
    /// polling.
    ///
    /// NOTE: the signal-watcher runs as a `tokio::spawn` task, not as a
    /// POSIX signal handler: `tokio::signal::unix::signal(SIGTERM)?.recv()`
    /// delivers the signal as an async value. No `async-signal-safe`
    /// constraints apply because we never touch tokio from a true signal
    /// handler context.
    pub shutdown_notify: Arc<Notify>,
    /// Handle to the per-target `Store`. Opened once at daemon startup in
    /// `daemon::run_active_daemon`; connection handlers access typed
    /// collection accessors via `ctx.store.plans()` without locking —
    /// `Store` methods take `&self` and the underlying `AsyncStore` is
    /// `Send + Sync`.
    ///
    /// **Shutdown ownership contract:** `Store::close` consumes `self`,
    /// so the daemon's shutdown path calls `Arc::try_unwrap` on the
    /// `Arc<DaemonContext>` to recover the owned `Store` and
    /// `close().await` it before the tokio runtime exits. For
    /// `try_unwrap` to succeed, every `Arc<DaemonContext>` clone (accept
    /// loop, signal watcher, handler tasks) MUST release its reference
    /// before the shutdown path's try_unwrap call — a stranded clone
    /// falls back to `Store::Drop`, whose sync writer-thread join can
    /// trigger tokio's "cannot block the current thread" panic on a
    /// runtime worker. The accept loop drains its handler `JoinSet`
    /// before returning, and the signal-watcher task is joined with a
    /// short timeout after the accept loop returns, specifically to make
    /// this contract hold.
    pub store: Arc<Store>,
    /// Per-transition summary-write decorator. Holds an `Arc::clone` of
    /// `store` for both the inner sink write-through and the
    /// c-extended Work-update path's parent-Plan + siblings reads.
    /// Constructed once at boot in `DaemonContext::new`.
    pub summary_fanout: Arc<crate::daemon::summary_fanout::SummaryFanout<Arc<Store>>>,
    /// Handle to the process-wide LLM client. Built once at daemon
    /// startup and shared across handler tasks via `Arc`. Generic on
    /// `L: LlmClient` so production can instantiate with
    /// `AnthropicClient` and tests can instantiate with a stub.
    /// Decompose call sites pass `&*ctx.llm` as the `&L`; the Arc
    /// deref produces `&L`, which implements the trait.
    pub llm: Arc<L>,
    /// Process-wide lane router. Enforces per-lane concurrency caps via
    /// tokio semaphores; wraps `Local`-lane Commands with bwrap when
    /// posture + detection allow. Shared into every `ToolContext` via
    /// `Arc` (not a trait-object handle; `LaneRouter` is a concrete type
    /// per the v5 no-`dyn`-for-DI rule).
    pub router: Arc<LaneRouter>,
    /// Base-plus-target bash denylist. Base patterns come from
    /// `BashDenylist::with_base()`; target-level patterns extend it from
    /// `.loopr/config.yml tools.bash-denylist-extend`.
    pub bash_denylist: Arc<BashDenylist>,
    /// Path patterns denied to Read/Write/Edit/Grep/Glob. Defaults (`.env`,
    /// `.key`, `.pem`, `credentials`, `secret`) get extended by
    /// `tools.path-deny-patterns` in the config.
    pub path_deny_patterns: Vec<String>,
    /// Sandbox posture. Recorded here for `tool_context()` to mirror into
    /// every built `ToolContext`. The daemon itself read this out of
    /// `config.tools.sandbox` at startup and used it to construct the
    /// router; we keep a copy so the ToolContext doesn't have to go
    /// through `router.sandbox_mode()` on every build.
    pub sandbox: SandboxMode,
    /// Prompt assembly. Shared by every implementer invocation; the
    /// inline impl is stateless. Arc'd into `agents::implementer::Deps`
    /// per Work.
    pub context_builder: Arc<InlineContextBuilder>,
    /// Configuration for the Implementer ralph loop (max_iterations,
    /// max_requeries, max_repeat_action, max_parse_failures). Cloned
    /// per Work.
    pub implementer_config: ImplementerConfig,
    /// Configuration for the Reviewer single-turn loop (max_requeries,
    /// diff_byte_cap, noop_files_byte_cap). Cloned per Bundle.
    pub reviewer_config: ReviewerConfig,
    /// Configuration for the Integrator (git_timeout, allow_multi_bundle).
    /// Cloned per Bundle-integration.
    pub integrator_config: IntegratorConfig,
    /// Configuration for the Director per-Plan supervisor (poll/idle
    /// intervals, restart cap, parse-failure cap, model, token budget).
    /// Cloned per Plan when `handle_plan_create` spawns the Director.
    pub director_config: DirectorConfig,
    /// Decomposition knobs (`max_children`). Passed by reference into
    /// `decomposer::decompose` on every `plan.create`.
    pub decomposer_config: decomposer::DecomposerConfig,
    /// Intra-daemon working-tree serializer. Shared into every
    /// `IntegratorDeps` so two concurrent `integrate` calls on the same
    /// target do not race on `git checkout` / `git merge`. First gate:
    /// one active integration per daemon; the lock is rarely contended.
    pub git_lock: Arc<Mutex<()>>,
    /// Post-Work worktree disposal policy. Read from config at startup;
    /// each `spawn_implementer_for_work` consults it after
    /// `run_implementer` returns to decide whether to destroy the
    /// worktree immediately, defer to run-end, or leak on purpose for
    /// debugging.
    pub worktree_cleanup_policy: AttemptCleanupPolicy,
    /// In-flight Implementer tasks. Drained on shutdown with a soft
    /// timeout + abort_all fallback; adds a third holder of the
    /// `Arc<DaemonContext>` clone (alongside accept-loop + signal-
    /// watcher) that MUST release before `Arc::try_unwrap` reclaims
    /// the Store for `.close().await`.
    pub implementer_tasks: Mutex<JoinSet<()>>,
    /// In-flight Reviewer tasks. Stage 8 wiring capstone: drained AFTER
    /// `implementer_tasks` at shutdown so in-flight Implementers can
    /// enqueue their Reviewer before the Reviewer drain begins.
    pub reviewer_tasks: Mutex<JoinSet<()>>,
    /// In-flight Integrator tasks. Drained AFTER `reviewer_tasks` at
    /// shutdown so in-flight Reviewers with an Accept verdict can
    /// enqueue their Integrator before the Integrator drain begins.
    pub integrator_tasks: Mutex<JoinSet<()>>,
    /// In-flight Director tasks (one per active Plan). Drained AFTER
    /// `integrator_tasks` at shutdown so a Director that triggers a
    /// final Integrator can finish before the Director itself winds
    /// down. Spawned by `handle_plan_create` and (after a daemon
    /// restart) `startup_reconcile_directors`.
    pub director_tasks: Mutex<JoinSet<()>>,
    /// In-flight `WorkSpawner`-issued tasks. The Director's
    /// `accept_bundle`, `override_work`, and `assign_work` calls each
    /// spawn into this pool so shutdown can drain them deterministically
    /// (vs. v0.7.11's bare `tokio::spawn`, whose handles were never
    /// joined). Drained AFTER `director_tasks` and BEFORE
    /// `integrator_tasks` per the spawn DAG: the Director feeds this
    /// pool, this pool feeds the Integrator pool. See `daemon.rs`'s
    /// drain rationale comment for the full case.
    pub work_spawner_tasks: Mutex<JoinSet<()>>,
    /// In-flight `plan.create` decompose tasks. `handle_plan_create` ACKs
    /// the client immediately after persisting the Plan, then runs
    /// decompose + Works persist + initial Implementer/Director spawns on
    /// a task in this pool. Drained FIRST at shutdown (root of the spawn
    /// DAG) so its children reach their own pools before those drain. See
    /// `daemon.rs::drain_plan_create_tasks`.
    pub plan_create_tasks: Mutex<JoinSet<()>>,
    /// Phase 2 sidecar map: live Implementer tasks indexed by `WorkId`.
    /// Inserted at the top of `spawn_implementer_for_work`'s body via a
    /// `ScopedIdGuard`; removed on `Drop` (panic or success).
    /// `WorkSpawner::list_running_work_ids` reads the keys to support
    /// `reconcile_director`'s detection of `InProgress` Works whose
    /// Implementer panicked. Uses `std::sync::RwLock` (sync) so the
    /// `WorkSpawner` trait's sync `list_running_*_ids` methods do not
    /// need an async bridge.
    pub implementer_work_ids: Arc<StdRwLock<HashMap<WorkId, ()>>>,
    /// Phase 18 (verified-swarm): keyed cancellation handles for live
    /// Implementer tasks, indexed by `WorkId`. A `JoinSet` has no
    /// per-task abort-by-key, so the daemon records each Implementer's
    /// `AbortHandle` at spawn (`spawn_implementer_registered`, holding
    /// this lock across the `JoinSet::spawn` + insert). The
    /// `work.override` handler fires the handle to abort an in-flight
    /// Work (`InProgress -> Blocked`); the spawn future's drop-path reaper
    /// then tears down the subprocess tree. Entries are pruned when their
    /// task finishes (`prune_finished_abort_handles`, called by the pool
    /// reaper). `std::sync::Mutex`: every access is a short, non-async
    /// insert/remove/prune, never held across an `.await`.
    pub implementer_abort_handles: Arc<StdMutex<HashMap<WorkId, AbortHandle>>>,
    /// Phase 2 sidecar map: live Reviewer tasks indexed by `BundleId`.
    pub reviewer_bundle_ids: Arc<StdRwLock<HashMap<BundleId, ()>>>,
    /// Phase 2 sidecar map: live Integrator tasks indexed by `BundleId`.
    pub integrator_bundle_ids: Arc<StdRwLock<HashMap<BundleId, ()>>>,
    /// Phase 9 (Director Phase 2): per-Plan operator-note wake-up
    /// channels. The `director.chat` IPC handler resolves the Plan's
    /// `Arc<Notify>` from this map and calls `notify_one()` after the
    /// note is persisted, preempting the Director's inter-iteration
    /// sleep. Inserts happen in `handle_plan_create` and
    /// `startup_reconcile_directors`; removal happens in
    /// `transition_and_persist_plan` when the Plan reaches a terminal
    /// state. Using `tokio::sync::RwLock` so the async chat handler
    /// can `await` while reading.
    pub operator_notifies: Arc<RwLock<HashMap<PlanId, Arc<Notify>>>>,
    /// Director Phase 2 follow-ups (Item 3): per-Plan status sidecar.
    /// The Director task writes a `DirectorStatusSnapshot` at the end
    /// of every iteration; the `director.status` IPC handler reads
    /// the snapshot to surface live mode + streak data to operators.
    /// Inserts happen at the first iteration's write; removal happens
    /// in the Director task body on exit (terminal Plan transition or
    /// daemon shutdown), mirroring `operator_notifies`. Sync RwLock
    /// matches the existing `implementer_work_ids` / `reviewer_bundle_ids`
    /// / `integrator_bundle_ids` sidecar lock discipline; held for one
    /// `HashMap::insert` per write, never across an `.await`.
    pub director_statuses: Arc<StdRwLock<HashMap<PlanId, DirectorStatusSnapshot>>>,
    /// Per-process counter snapshot. Held in a `std::sync::Mutex`
    /// because every emitter is short, non-async, and the value is
    /// shared with the panic hook and SIGQUIT handler (both of which
    /// run on threads that aren't part of the tokio reactor and
    /// can't .await). Phase 7 of the Tier-1 cleanup wires this in;
    /// the daemon writes a per-process digest at exit.
    pub snapshot: Arc<StdMutex<ProcessSnapshot>>,
    /// Server-side IPC timeouts (idle + write). Built from
    /// `config.transport` at boot; read by every `handle_client` task.
    pub server_timeouts: crate::transport::ServerTimeouts,
    /// Per-run cumulative LLM cost cap in U.S. dollars (`budgets.
    /// per-run-cost-usd`). `None` = unlimited. Checked at the spawn gates
    /// (`spawn_implementer_for_work`, `spawn_director_for_plan`) against
    /// the live `ProcessSnapshot` cost; on breach the daemon stops
    /// spawning new agents (soft pause, vision Budgets).
    pub per_run_cost_usd: Option<f64>,
    /// One-shot guard so the per-run budget breach emits exactly one
    /// `budget.exceeded` event no matter how many spawn attempts the
    /// reactor makes after the cap is hit. Cleared by `budget.reset`
    /// (Phase 15) so a tripped daemon can resume without a restart once
    /// the operator raises the cap.
    budget_event_sent: AtomicBool,
    /// Phase 15 (`docs/design/2026-07-11-verified-swarm.md`): global
    /// implementer semaphore bounding the N-plans x M-works LLM
    /// fan-out. A permit is acquired at the TOP of
    /// `spawn_implementer_for_work`, before any other guard, and
    /// released as soon as `run_implementer` returns — well before the
    /// Reviewer spawn, which is never semaphore-bound. Sized from
    /// `budgets.max-concurrent-implementers`
    /// (`config::DEFAULT_MAX_CONCURRENT_IMPLEMENTERS` when unset).
    implementer_semaphore: Semaphore,
}

impl<L: LlmClient + Send + Sync + 'static> DaemonContext<L> {
    /// Construct a new context. All fields are set once at daemon startup;
    /// nothing mutable is exposed except the `shutting_down` atomic.
    /// Takes an already-opened `Store`, a pre-built LLM client, and
    /// already-built tool infrastructure (`LaneRouter`, `BashDenylist`,
    /// `path_deny_patterns`, `SandboxMode`). Router construction can fail
    /// when `SandboxMode::Required` + no bwrap; that failure happens in
    /// `run_active_daemon` before this constructor, so this function is
    /// infallible.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: PathBuf,
        session_id: SessionId,
        target_slug: String,
        process_id: ProcessId,
        pid: u32,
        store: Store,
        llm: Arc<L>,
        router: Arc<LaneRouter>,
        bash_denylist: Arc<BashDenylist>,
        path_deny_patterns: Vec<String>,
        sandbox: SandboxMode,
        context_builder: Arc<InlineContextBuilder>,
        implementer_config: ImplementerConfig,
        reviewer_config: ReviewerConfig,
        integrator_config: IntegratorConfig,
        director_config: DirectorConfig,
        decomposer_config: decomposer::DecomposerConfig,
        worktree_cleanup_policy: AttemptCleanupPolicy,
        snapshot: Arc<StdMutex<ProcessSnapshot>>,
        server_timeouts: crate::transport::ServerTimeouts,
        per_run_cost_usd: Option<f64>,
        max_concurrent_implementers: usize,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENTS_CAPACITY);
        let store = Arc::new(store);
        let summary_fanout = Arc::new(crate::daemon::summary_fanout::SummaryFanout::with_events(
            Arc::clone(&store),
            target.clone(),
            Arc::clone(&store),
            events.clone(),
        ));
        Self {
            target,
            session_id,
            target_slug,
            process_id,
            started_at: chrono::Local::now(),
            pid,
            events,
            shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
            store,
            summary_fanout,
            llm,
            router,
            bash_denylist,
            path_deny_patterns,
            sandbox,
            context_builder,
            implementer_config,
            reviewer_config,
            integrator_config,
            director_config,
            decomposer_config,
            git_lock: Arc::new(Mutex::new(())),
            worktree_cleanup_policy,
            implementer_tasks: Mutex::new(JoinSet::new()),
            reviewer_tasks: Mutex::new(JoinSet::new()),
            integrator_tasks: Mutex::new(JoinSet::new()),
            director_tasks: Mutex::new(JoinSet::new()),
            work_spawner_tasks: Mutex::new(JoinSet::new()),
            plan_create_tasks: Mutex::new(JoinSet::new()),
            implementer_work_ids: Arc::new(StdRwLock::new(HashMap::new())),
            implementer_abort_handles: Arc::new(StdMutex::new(HashMap::new())),
            reviewer_bundle_ids: Arc::new(StdRwLock::new(HashMap::new())),
            integrator_bundle_ids: Arc::new(StdRwLock::new(HashMap::new())),
            operator_notifies: Arc::new(RwLock::new(HashMap::new())),
            director_statuses: Arc::new(StdRwLock::new(HashMap::new())),
            snapshot,
            server_timeouts,
            per_run_cost_usd,
            budget_event_sent: AtomicBool::new(false),
            implementer_semaphore: Semaphore::new(max_concurrent_implementers),
        }
    }

    /// True when a per-run cost cap is configured and the live process
    /// cost has reached it. `None` cap = always false (unlimited).
    fn run_budget_exceeded(&self) -> bool {
        let Some(cap_usd) = self.per_run_cost_usd else {
            return false;
        };
        let cost_micros = self.snapshot.lock().map(|s| s.llm_cost_micros).unwrap_or(0);
        let cap_micros = (cap_usd * 1_000_000.0) as u64;
        cost_micros >= cap_micros
    }

    /// Soft-pause gate for the spawn paths. Returns `true` (caller must
    /// NOT spawn) when the per-run budget is exhausted, emitting exactly
    /// one `budget.exceeded` event on the first breach. `role`/`id` name
    /// the spawn that was suppressed, for the log line.
    pub(crate) fn budget_blocks_spawn(&self, role: &str, id: &str) -> bool {
        if !self.run_budget_exceeded() {
            return false;
        }
        let cost = self.snapshot.lock().map(|s| s.llm_cost_micros).unwrap_or(0);
        // Emit the event once; subsequent suppressed spawns log at debug.
        if self
            .budget_event_sent
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            let cap = self.per_run_cost_usd.unwrap_or(0.0);
            tracing::warn!(
                role,
                id,
                cost_usd = cost as f64 / 1_000_000.0,
                cap_usd = cap,
                "per-run budget exceeded; soft-pausing new agent spawns"
            );
            let _ = self.events.send(DaemonEvent {
                event: "budget.exceeded".to_string(),
                data: serde_json::json!({
                    "scope": "per-run",
                    "cost_usd": cost as f64 / 1_000_000.0,
                    "cap_usd": cap,
                }),
            });
        } else {
            tracing::debug!(role, id, "spawn suppressed: per-run budget already exceeded");
        }
        true
    }

    /// `budget.reset` IPC verb body (Phase 15). Clears the one-shot
    /// `budget_event_sent` soft-pause guard so a budget-tripped daemon
    /// resumes dispatching new implementer spawns on its next reactive
    /// sweep, without a restart. Returns the PRIOR value (`true` = the
    /// daemon had actually tripped, so this reset mattered; `false` = the
    /// guard was already clear, a no-op). Does not touch
    /// `per_run_cost_usd` itself — the operator is expected to raise the
    /// cap (via a new `.loopr/config.yml` + daemon restart, since
    /// `per_run_cost_usd` is read once at `DaemonContext::new`) before
    /// calling this, or the very next spawn attempt re-trips the guard.
    pub fn reset_budget_event(&self) -> bool {
        self.budget_event_sent.swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// Build a per-invocation `ToolContext` for one tool call.
    ///
    /// The persist base for overflow output is
    /// `<xdg>/loopr/sessions/<session-id>/targets/<slug>/runs/<process-id>/work/<work-id>/`.
    /// The ralph loop
    /// creates these directories on first use; the tool subprocess spawner
    /// calls `create_dir_all` before writing.
    ///
    /// Stage 7's implementer design doc consumes this helper; Phase 4 wires
    /// it up so the `tools` crate has a live caller even before the agent
    /// loop lands.
    /// Spawn an Implementer task into `implementer_tasks` AND register its
    /// `AbortHandle` under the Work's id, so the `work.override` operator
    /// verb can abort that specific in-flight Work (`InProgress ->
    /// Blocked`). Phase 18 of `docs/design/2026-07-11-verified-swarm.md`.
    ///
    /// The JoinSet lock is held across the `spawn` + the abort-map insert
    /// so the handle is registered before the task can finish and try to
    /// prune itself — the pool reaper's `prune_finished_abort_handles`
    /// removes finished entries, so a task that completes before an
    /// operator aborts leaves at most one stale (harmless, no-op-on-fire)
    /// handle until the next reap. Every `implementer_tasks.spawn` site
    /// routes through here so no dispatch escapes keyed cancellation.
    pub async fn spawn_implementer_registered(self: &Arc<Self>, work: Work) {
        let wid = work.id.clone();
        let mut tasks = self.implementer_tasks.lock().await;
        let handle = tasks.spawn(Arc::clone(self).spawn_implementer_for_work(work));
        match self.implementer_abort_handles.lock() {
            Ok(mut m) => {
                m.insert(wid, handle);
            }
            Err(_) => {
                warn!(work_id = %wid, "implementer_abort_handles poisoned; abort handle not registered");
            }
        }
    }

    /// Drop `AbortHandle` entries whose Implementer task has finished, so
    /// the keyed-cancellation map does not grow unbounded across a long
    /// run. Called by the background pool reaper (`reap_all_pools`) on the
    /// same cadence it reaps finished JoinSet tasks. `is_finished()` is the
    /// race-free liveness signal: a just-inserted handle for a task that
    /// already completed reads finished and is pruned next sweep, so the
    /// insert-then-finish ordering never leaks. Firing a finished handle is
    /// a harmless no-op, so a between-sweeps stale entry costs nothing but
    /// a map slot.
    pub fn prune_finished_abort_handles(&self) {
        match self.implementer_abort_handles.lock() {
            Ok(mut m) => m.retain(|_, handle| !handle.is_finished()),
            Err(_) => warn!("implementer_abort_handles poisoned; skipping prune"),
        }
    }

    /// Drive one Work through the Implementer loop inside its own
    /// worktree, persisting the resulting Bundle (happy path) or
    /// transitioning the Work record to `Blocked` (error path).
    ///
    /// Takes `Arc<Self>` so the method body can be moved into a
    /// `tokio::spawn` task owned by `self.implementer_tasks`. All
    /// inputs to `run_implementer` are assembled from this context:
    /// LLM client, tool registry, Store as BundleSink, context builder,
    /// and config. The worktree is sandboxed at
    /// `<target>/.loopr/worktrees/<work-id>-<seq>/`.
    ///
    /// Sync git and worktree ops run inside `tokio::task::spawn_blocking`
    /// per vision.md:134 so they don't starve the tokio reactor.
    ///
    /// Cleanup honors `AttemptCleanupPolicy`. The worktree's branch is
    /// always retained regardless of cleanup policy (vision.md:135 —
    /// Stage 8 Integrator will merge it).
    #[instrument(
        name = "daemon.spawn_implementer_for_work",
        level = "info",
        skip_all,
        fields(work_id = %work.id, work_status = ?work.status, session_id = %self.session_id),
    )]
    pub async fn spawn_implementer_for_work(self: Arc<Self>, mut work: Work) {
        // Phase 15 global implementer semaphore (bounds the N-plans x
        // M-works LLM fan-out): acquired FIRST, before any other guard,
        // per the design doc's "top of spawn_implementer_for_work"
        // placement. Never closed in production (no `close()` caller),
        // so `acquire()` failing would mean the semaphore was dropped out
        // from under a live `Arc<Self>` — unreachable; `.expect` documents
        // the invariant rather than threading a dead error arm. Released
        // explicitly the moment `run_implementer` returns, well before the
        // Reviewer spawn below (the Reviewer/Integrator are NOT
        // semaphore-bound).
        let _permit = self
            .implementer_semaphore
            .acquire()
            .await
            .expect("implementer semaphore is never closed");

        // Phase 2 sidecar-map insert. Guard is dropped on every exit
        // (panic or normal return), so `WorkSpawner::list_running_work_ids`
        // observes the live set even across abrupt panics.
        let _id_guard = ScopedIdGuard::new(Arc::clone(&self.implementer_work_ids), work.id.clone());

        // Shutdown drain guard: a spawn that slipped past a caller's
        // pre-spawn check (e.g. `promote_unblocked_siblings`) after the
        // signal landed must not do work into an already-drained pool.
        // Mirrors `spawn_reviewer_for_bundle` / `spawn_integrator_for_bundle`
        // so no in-flight body outlives the drain and strands an
        // `Arc<DaemonContext>` clone against `Arc::try_unwrap`.
        if self.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
            debug!("shutdown in progress; skipping implementer spawn");
            return;
        }

        // Budget soft pause (vision Budgets): if the per-run cost cap is
        // exhausted, do not spawn. The Work stays Pending (no FSM advance
        // below) and is re-driven cheaply on later sweeps; in-flight work
        // finishes. The first breach emits one `budget.exceeded` event.
        if self.budget_blocks_spawn("implementer", work.id.as_ref()) {
            return;
        }

        // Advance Work through the pipeline-start transitions via the FSM.
        // Guarded: reconcile or a prior call may have advanced us already.
        if work.status == WorkStatus::Pending
            && let Err(e) = transition_and_persist_work(
                &*self.summary_fanout,
                &mut work,
                WorkStatus::Ready,
                Role::Reactor,
                false,
                &self.snapshot,
            )
            .await
        {
            error!(error = %e, "Pending -> Ready transition failed; abandoning task");
            return;
        }
        if work.status == WorkStatus::Ready
            && let Err(e) = transition_and_persist_work(
                &*self.summary_fanout,
                &mut work,
                WorkStatus::InProgress,
                Role::Reactor,
                false,
                &self.snapshot,
            )
            .await
        {
            error!(error = %e, "Ready -> InProgress transition failed; abandoning task");
            return;
        }

        let sha = match rev_parse_head(&self.target).await {
            Ok(sha) => sha,
            Err(e) => {
                error!(error = %e, "sha lookup failed; Work remains InProgress");
                return;
            }
        };

        let worktree_root = self.target.join(".loopr").join("worktrees");
        let persist_base = match telemetry::session_run_dir(&self.session_id, &self.target_slug, &self.process_id) {
            Ok(run_dir) => run_dir.join("work").join(work.id.as_ref()),
            Err(e) => {
                error!(error = %e, "session_run_dir failed; Work remains InProgress");
                return;
            }
        };
        let _ = std::fs::create_dir_all(&persist_base);

        let target = self.target.clone();
        let root = worktree_root.clone();
        let wid = work.id.clone();
        let base = sha.clone();
        let worktree = match tokio::task::spawn_blocking(move || Worktree::create(&target, &root, wid, &base)).await {
            Ok(Ok(wt)) => wt,
            Ok(Err(e)) => {
                error!(error = %e, "Worktree::create failed");
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    false,
                    &self.snapshot,
                )
                .await;
                self.wake_director(&work.parent_id).await;
                return;
            }
            Err(e) => {
                error!(error = %e, "spawn_blocking join error on Worktree::create");
                return;
            }
        };

        let tools = RealTools::new(
            self.router.clone(),
            self.sandbox,
            self.bash_denylist.clone(),
            self.path_deny_patterns.clone(),
            Some(persist_base),
        );
        let tool_schemas = ::tools::all_schemas();

        // F8/bullet 8: thread the most-recent Rejected Bundle's reviewer
        // feedback into the retry Implementer's StateSummary so it learns WHY
        // the prior bundle was rejected. Pre-fix this was hardcoded
        // `StateSummary::default()`, severing the doom-loop feedback channel.
        //
        // Phase 11: the feedback is assembled from the persisted `Review`'s
        // STRUCTURED reasons (capped), not the one-line `Bundle.verification`
        // string. The full Review stays on disk; the prompt gets the issue
        // list. Fall back to the verification one-liner only when no Review is
        // on record (pre-Phase-11 rows or a crash between the two writes).
        let rejected_bundle_reason = match self.store.bundles().list_by_work_id(&work.id).await {
            Ok(bundles) => {
                let latest_rejected = bundles
                    .into_iter()
                    .filter(|b| b.status == BundleStatus::Rejected)
                    .max_by_key(|b| b.updated_at);
                match latest_rejected {
                    Some(b) => match self.store.reviews().list_by_bundle(&b.id).await {
                        Ok(reviews) => reviews
                            .iter()
                            .max_by_key(|r| r.round)
                            .and_then(render_review_feedback)
                            .or_else(|| Some(b.verification.clone()).filter(|v| !v.trim().is_empty())),
                        Err(e) => {
                            warn!(
                                error = %e, work_id = %work.id, bundle_id = %b.id,
                                "review feedback lookup failed; falling back to verification string"
                            );
                            Some(b.verification).filter(|v| !v.trim().is_empty())
                        }
                    },
                    None => None,
                }
            }
            Err(e) => {
                warn!(error = %e, work_id = %work.id, "rejected-bundle feedback lookup failed; retrying without it");
                None
            }
        };

        let deps = Deps {
            llm: Arc::clone(&self.llm),
            tools,
            bundles: &self.store,
            context: Arc::clone(&self.context_builder),
            config: self.implementer_config.clone(),
            tool_schemas,
            state: StateSummary { rejected_bundle_reason },
            run_id: Some(self.process_id.to_string()),
        };

        // Panic posture (vision.md "Failure posture"). Wrap the
        // implementer future in `catch_unwind` so a panic inside
        // `run_implementer` does NOT abort this task before the
        // worktree-cleanup tail below — a panicking implementer used to
        // leak its worktree (the JoinSet swallowed the panic). On panic
        // we record `FailureReason::Panic`, mark the Work Blocked, and
        // fall through to the same cleanup tail as every other arm.
        // Cost attribution: install the per-call context so the metered
        // client's costs.jsonl lines carry this Plan/Work/role.
        let call_ctx = llm::CallContext {
            plan_id: Some(work.parent_id.to_string()),
            work_id: Some(work.id.to_string()),
            role: Some("implementer".to_string()),
        };
        let result = std::panic::AssertUnwindSafe(llm::CallContext::scope(
            call_ctx,
            run_implementer(&work, &worktree, &deps),
        ))
        .catch_unwind()
        .await;

        // Phase 15: release the implementer permit the instant the
        // implementer run returns (or panics) — BEFORE routing the
        // outcome, BEFORE the terminal FSM transition below, and BEFORE
        // the Reviewer spawn inside the `Ok(Ok(bundle))` arm. The
        // Reviewer and Integrator are never semaphore-bound; only the
        // Implementer's own LLM fan-out is capped.
        drop(_permit);

        match result {
            Err(panic) => {
                let msg = panic_message(&*panic);
                error!(panic = %msg, "implementer panicked; marking Work Blocked (FailureReason::Panic)");
                work.session_failure_count = work.session_failure_count.saturating_add(1);
                work.failure_reason = Some(FailureReason::Panic);
                work.blocked_reason = Some(format!("implementer panicked: {msg}"));
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    false,
                    &self.snapshot,
                )
                .await;
            }
            Ok(Ok(bundle)) => {
                // Phase 6: the canonical "implementer produced bundle" event
                // is emitted from `agents::dispatch::propose_bundle` with
                // the full paths/patch_id manifest. The daemon-context
                // site is intentionally silent here to avoid a duplicate
                // log line on a different target/span ancestry. See
                // docs/design/2026-05-09-comprehensive-telemetry.md Phase 6.
                // `Role::Implementer` as identifier: daemon fires the FSM
                // transition on the Implementer's behalf once `run_implementer`
                // returns Ok. The FSM's authored-edge table lists this as
                // `InProgress -> InReview by (Implementer)`, which is exactly
                // the semantic we want; the "Role as identifier" invariant
                // from the Reviewer doc generalizes here.
                if let Err(e) = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::InReview,
                    Role::Implementer,
                    false,
                    &self.snapshot,
                )
                .await
                {
                    error!(error = %e, "InProgress -> InReview transition failed after successful implementer");
                }
                // Phase 4: the daemon has a freshly-produced Bundle in hand
                // here regardless of the transition outcome above — this is
                // the loopr-side observation point for "a Bundle was
                // proposed" (the actual `Bundle` record is created inside
                // `agents::dispatch::propose_bundle`, out of this phase's
                // crate scope, so the counter is wired at the closest
                // in-scope seam instead).
                if let Ok(mut snap) = self.snapshot.lock() {
                    snap.bundles_proposed += 1;
                } else {
                    warn!("spawn_implementer_for_work: snapshot Mutex poisoned; bundles_proposed dropped");
                }
                // Stage 8 Phase 2 handoff: spawn a Reviewer task for this
                // Bundle. Shutdown-guard: if the daemon is winding down, skip
                // the spawn so the reviewer-tasks drain does not race.
                if !self.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                    let reviewer_ctx = Arc::clone(&self);
                    let mut rts = self.reviewer_tasks.lock().await;
                    rts.spawn(reviewer_ctx.spawn_reviewer_for_bundle(bundle));
                }
            }
            Ok(Err(ImplementerError::EscalationNeeded(reason))) => {
                warn!(%reason, "implementer escalated; marking Work Blocked");
                work.session_failure_count = work.session_failure_count.saturating_add(1);
                work.failure_reason = Some(FailureReason::Other(reason.clone()));
                work.blocked_reason = Some(reason);
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    false,
                    &self.snapshot,
                )
                .await;
            }
            Ok(Err(other)) => {
                let detail = other.to_string();
                error!(error = %detail, "implementer error; marking Work Blocked");
                work.session_failure_count = work.session_failure_count.saturating_add(1);
                work.failure_reason = Some(FailureReason::Other(detail.clone()));
                work.blocked_reason = Some(detail);
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    false,
                    &self.snapshot,
                )
                .await;
            }
        }

        // Phase A: if the implementer escalated or errored the Work to
        // Blocked, wake the Director to run recovery now instead of waiting
        // out its idle poll (no-op for the Ok -> InReview path).
        if work.status == WorkStatus::Blocked {
            self.wake_director(&work.parent_id).await;
        }

        // Phase 10: when the implementer produced a Bundle (Work -> InReview),
        // keep the worktree WARM so the Reviewer's executed checks run against
        // incremental build caches, not a cold recreate that trips subprocess
        // timeouts. Cleanup is deferred until the Bundle reaches a terminal
        // state — Phase 19 reaps at terminal states; here we simply do not
        // delete at the old (too-early) point. `retain()` marks the handle
        // consumed so its `Drop` safety-net does not remove the worktree
        // either. On any non-review exit (Blocked), clean up now as before.
        if work.status == WorkStatus::InReview {
            debug!("implementer produced bundle; retaining worktree warm for reviewer checks (cleanup deferred)");
            worktree.retain();
        } else {
            match self.worktree_cleanup_policy {
                AttemptCleanupPolicy::Immediate | AttemptCleanupPolicy::OnWorkTerminal => {
                    let _ = tokio::task::spawn_blocking(move || worktree.cleanup()).await;
                }
                AttemptCleanupPolicy::OnRunEnd => {}
                AttemptCleanupPolicy::Never => {
                    warn!("AttemptCleanupPolicy::Never — leaking worktree (debug only)");
                }
            }
        }
    }

    /// Review a persisted Bundle. Triages `Proposed -> Triaged`, repairs
    /// the Work status if reconcile surfaced it at `InProgress`, runs the
    /// Reviewer LLM turn, and routes the Verdict. On `Accept` the Bundle
    /// transitions `Reviewed -> Accepted`; Phase 3 spawns the Integrator
    /// from that branch. On `ChangeRequested` / `Reject` / Err, the Work
    /// transitions to `Blocked` via the new one-step override edge and
    /// no further stage runs (first-gate: no Director, no re-implementer).
    ///
    /// Shutdown-aware: early-returns if the daemon is winding down, so a
    /// signal arriving during reviewer dispatch does not spawn a task that
    /// will never drain. Per `CLAUDE.md` agents crate rule, every
    /// orchestration decision (triage, verdict routing, next-stage spawn)
    /// lives here, not in `agents::reviewer`.
    #[instrument(
        name = "daemon.spawn_reviewer_for_bundle",
        level = "info",
        skip_all,
        fields(bundle_id = %bundle.id, work_id = %bundle.work_id, bundle_status = ?bundle.status, session_id = %self.session_id),
    )]
    pub async fn spawn_reviewer_for_bundle(self: Arc<Self>, mut bundle: Bundle) {
        // Phase 2 sidecar-map insert; mirrors `spawn_implementer_for_work`.
        let _id_guard = ScopedIdGuard::new(Arc::clone(&self.reviewer_bundle_ids), bundle.id.clone());

        if self.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
            debug!("shutdown in progress; skipping reviewer spawn");
            return;
        }

        // Step 1: triage Proposed -> Triaged. OCC-aware; a second triage
        // of the same Bundle (reconcile + Implementer hand-off racing)
        // returns Stale and we exit cleanly. Routed through
        // `transition_and_persist_bundle` (docs/design/2026-07-12-
        // reviewer-occ-stale-race.md) so the floored `updated_at` this
        // write returns is re-synced onto `bundle` before `run_reviewer`
        // snapshots its own OCC token from it.
        if let Err(e) =
            transition_and_persist_bundle(&*self.summary_fanout, &mut bundle, BundleStatus::Triaged, Role::Reactor)
                .await
        {
            match e {
                BundleTransitionError::Fsm(_) => {
                    error!(error = %e, "bundle Proposed -> Triaged transition rejected by FSM; skipping");
                }
                BundleTransitionError::Stale { .. } | BundleTransitionError::Persist(_) => {
                    warn!(error = %e, "triage OCC update failed (another task beat us?); skipping");
                }
            }
            return;
        }

        // Step 2: load Work.
        let mut work = match self.store.works().get(&bundle.work_id).await {
            Ok(w) => w,
            Err(e) => {
                error!(error = %e, "work lookup failed during review; skipping");
                return;
            }
        };

        // Step 3: Work state repair BEFORE run_reviewer. If reconcile
        // spawned us against a Bundle whose Implementer crashed before
        // firing InProgress -> InReview, pull the Work up now so the
        // store is consistent for the full review window.
        match work.status {
            WorkStatus::InReview => {}
            WorkStatus::InProgress => {
                // F8 (same class): route through the SummaryFanout sink
                // like every other writer so the Work summary refreshes
                // with the repair; the raw `&self.store` here skipped it.
                if let Err(e) = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::InReview,
                    Role::Reactor,
                    true, // override
                    &self.snapshot,
                )
                .await
                {
                    error!(error = %e, work_status = ?work.status, "InProgress -> InReview repair failed");
                    return;
                }
            }
            WorkStatus::Blocked => {
                // Bullet 15: the Work went Blocked (a prior bundle's
                // rejection, or a dep terminalized it). A Triaged Bundle
                // whose Work is Blocked would be re-driven by every
                // recovery sweep forever — the reviewer exits here, so the
                // Bundle never reaches a terminal state. Supersede it
                // (Triaged -> Superseded by Reactor) so the sweep stops.
                warn!("Work is Blocked at reviewer entry; superseding Bundle to stop recovery re-drive");
                bundle.failure_reason = Some(FailureReason::Other("work blocked; bundle superseded".to_string()));
                // Routed through `transition_and_persist_bundle` (same helper
                // as the triage step) so this hand-rolled site can no longer
                // discard the floored `updated_at` (docs/design/2026-07-12-
                // reviewer-occ-stale-race.md). Benign here — this arm returns
                // immediately — but converted for hygiene: no raw
                // `.bundles().update` sequence survives in the daemon.
                if let Err(e) = transition_and_persist_bundle(
                    &*self.summary_fanout,
                    &mut bundle,
                    BundleStatus::Superseded,
                    Role::Reactor,
                )
                .await
                {
                    match e {
                        BundleTransitionError::Fsm(_) => {
                            error!(error = %e, "Bundle -> Superseded rejected by FSM; skipping");
                        }
                        BundleTransitionError::Stale { .. } | BundleTransitionError::Persist(_) => {
                            warn!(error = %e, "Bundle supersede OCC update failed (another task beat us?); skipping");
                        }
                    }
                }
                return;
            }
            other => {
                warn!(?other, "unexpected Work status at reviewer entry; skipping");
                return;
            }
        }

        // Step 4: build ReviewerDeps.
        // Phase 10: resolve the checkout the executed checks run in. The
        // implementer worktree is kept warm past review (cleanup deferred in
        // `spawn_implementer_for_work`) so build caches stay warm; use it when
        // present. If it is missing (crash), recreate an ephemeral worktree
        // from the bundle branch and flag it so the CheckRun excerpt records
        // the cold-cache caveat. `_ephemeral_guard` cleans up the ephemeral
        // worktree on scope exit (any return path).
        let (checkout_path, ephemeral_checkout, _ephemeral_guard) = self.resolve_review_checkout(&bundle).await;
        let check_runner: Arc<dyn CheckRunner> = Arc::new(ProductionCheckRunner::new(self.router.clone(), None));

        // Phase 6: pass the SummaryFanout decorator as the BundleUpdateSink
        // so per-Bundle summaries land transactionally with the OCC
        // update; the inner sink is `Arc<Store>` and the decorator's
        // BundleUpdateSink impl writes the summary on Ok. It also carries
        // the CheckRunSink impl for Phase 10 executed-check persistence.
        let deps = ReviewerDeps {
            llm: Arc::clone(&self.llm),
            store: &*self.summary_fanout,
            context: Arc::clone(&self.context_builder),
            config: self.reviewer_config.clone(),
            target: self.target.clone(),
            checkout_path,
            ephemeral_checkout,
            check_runner,
            path_deny_patterns: self.path_deny_patterns.clone(),
        };

        // Step 5: single LLM turn (plus bounded parse-retry inside run_reviewer).
        // Panic posture: `catch_unwind` so a panic inside `run_reviewer`
        // records `FailureReason::Panic`, Blocks the Work, and wakes the
        // Director instead of silently killing this reviewer task (the
        // JoinSet would otherwise swallow it). The Bundle stays Triaged
        // and is superseded by the next recovery sweep's Work-Blocked
        // entry guard.
        let call_ctx = llm::CallContext {
            plan_id: Some(work.parent_id.to_string()),
            work_id: Some(work.id.to_string()),
            role: Some("reviewer".to_string()),
        };
        let reviewer_result =
            std::panic::AssertUnwindSafe(llm::CallContext::scope(call_ctx, run_reviewer(&bundle, &work, &deps)))
                .catch_unwind()
                .await;
        let verdict = match reviewer_result {
            Err(panic) => {
                let msg = panic_message(&*panic);
                error!(panic = %msg, "reviewer panicked; marking Work Blocked (FailureReason::Panic)");
                work.failure_reason = Some(FailureReason::Panic);
                work.blocked_reason = Some(format!("reviewer panicked: {msg}"));
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                    &self.snapshot,
                )
                .await;
                self.wake_director(&work.parent_id).await;
                return;
            }
            Ok(Ok(v)) => v,
            // Phase 10 failure taxonomy: a SPAWN-level check failure (command
            // not found / exec failure) is an ENVIRONMENT problem, not a code
            // signal. The Work goes Blocked with a `blocked_reason` naming the
            // command; there was no LLM turn and no ChangeRequested — asking
            // the LLM to fix infra would burn `max_work_attempts` at max cost.
            Ok(Err(ReviewerError::CheckEnvironment { command, detail })) => {
                warn!(
                    %command,
                    %detail,
                    "reviewer: check environment failure; Work -> Blocked (no LLM turn, no ChangeRequested)"
                );
                work.failure_reason = Some(FailureReason::Other(format!("check environment failure: {command}")));
                work.blocked_reason = Some(format!(
                    "configured check `{command}` could not be spawned (environment): {detail}"
                ));
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                    &self.snapshot,
                )
                .await;
                self.wake_director(&work.parent_id).await;
                return;
            }
            Ok(Err(ReviewerError::EscalationNeeded(reason))) => {
                warn!(%reason, "reviewer escalated; marking Work Blocked");
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                    &self.snapshot,
                )
                .await;
                self.wake_director(&work.parent_id).await;
                return;
            }
            // F6: an OCC Stale on the reviewer's `Triaged -> Reviewed/Rejected`
            // write. Re-read the Bundle and discriminate the three real cases
            // rather than blanket-swallowing every Stale as "another reviewer
            // won" — a benign lost race leaves the winner's routing to stand
            // (never forcing the Work to Blocked, which would manufacture a
            // Bundle-Reviewed / Work-Blocked divergence), while a Bundle that
            // never advanced past `Triaged` is an OCC invariant violation with
            // no winner and fails loud. Byte-identical discrimination to
            // spawner.rs's accept_bundle Stale arm via the shared helper
            // (docs/design/2026-07-12-failure-paths-recovery-chain.md Phase 4).
            Ok(Err(ReviewerError::Update(store::BundleUpdateError::Stale { expected, actual }))) => {
                discriminate_stale_bundle_write(&self.store, &bundle.id, BundleStatus::Triaged, expected, actual).await;
                return;
            }
            Ok(Err(other)) => {
                let detail = other.to_string();
                error!(error = %detail, "reviewer error; marking Work Blocked");
                work.failure_reason = Some(FailureReason::Other(detail.clone()));
                work.blocked_reason = Some(detail);
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                    &self.snapshot,
                )
                .await;
                self.wake_director(&work.parent_id).await;
                return;
            }
        };

        // Step 6: route Verdict.
        match verdict {
            Verdict::Accept { summary } => {
                // Director Phase 3 handoff: the Bundle is now `Reviewed` and
                // stays there. Director's poll loop sees it, emits
                // `accept_bundle`, and `WorkSpawner::accept_bundle` fires
                // `Reviewed -> Accepted` + spawns Integrator. Stage 8's
                // inline auto-accept and Integrator spawn are intentionally
                // removed here (see docs/design/2026-05-08-director-phase-1.md
                // "Stage 8 Handoff: Bundle Acceptance").
                info!(
                    bundle_id = %bundle.id,
                    summary = %summary,
                    "reviewer accepted bundle; Bundle remains Reviewed for Director acceptance"
                );
            }
            Verdict::ChangeRequested { summary, reasons } => {
                warn!(summary = %summary, reason_count = reasons.len(), "reviewer requested changes; Work -> Blocked");
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                    &self.snapshot,
                )
                .await;
            }
            Verdict::Reject { reason } => {
                warn!(reason = %reason, "reviewer rejected bundle; Work -> Blocked");
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                    &self.snapshot,
                )
                .await;
            }
        }
        // Phase A: the verdict just moved the Bundle to Reviewed (Accept) or
        // the Work to Blocked (ChangeRequested/Reject) — both
        // Director-actionable. Wake the Director after the persists above so
        // it acts now instead of waiting out its idle poll (the measured
        // ~16.6s reviewer-accept gap).
        self.wake_director(&work.parent_id).await;
    }

    /// Resolve the checkout the Reviewer's executed checks run in. Returns
    /// `(checkout_path, ephemeral, guard)`:
    /// - the warm implementer worktree when present (`ephemeral = false`, no
    ///   guard — the worktree's lifetime is owned by the implementer task,
    ///   extended past review);
    /// - an ephemeral, detached worktree recreated from the bundle head when
    ///   the warm one is missing (crash) (`ephemeral = true`, the guard reaps
    ///   it on scope exit);
    /// - `self.target` as an inert fallback when checks are disabled or the
    ///   branch can't be resolved (checks either won't run or surface a spawn
    ///   error).
    async fn resolve_review_checkout(&self, bundle: &Bundle) -> (PathBuf, bool, Option<EphemeralCheckout>) {
        if self.reviewer_config.check_commands.is_empty() {
            // Checks disabled: the path is never used to execute anything.
            return (self.target.clone(), false, None);
        }
        let worktree_root = self.target.join(".loopr").join("worktrees");
        let Some((work_id, seq)) = worktree::parse_branch(&bundle.branch_name) else {
            warn!(
                branch = %bundle.branch_name,
                "reviewer checkout: unparseable bundle branch; falling back to target"
            );
            return (self.target.clone(), false, None);
        };
        let warm = worktree_root.join(format!("{work_id}-{seq}"));
        if warm.is_dir() {
            return (warm, false, None);
        }
        // Crash fallback: the warm worktree is gone. Recreate an ephemeral,
        // detached checkout from the bundle head so checks still run.
        warn!(
            work_id = %work_id,
            worktree = %warm.display(),
            "reviewer checkout: warm worktree missing; recreating ephemeral from bundle branch"
        );
        let ephemeral = worktree_root.join(format!("{work_id}-{seq}-review"));
        let reference = bundle.head_commit.clone().unwrap_or_else(|| bundle.branch_name.clone());
        match self.create_ephemeral_checkout(&ephemeral, &reference).await {
            Ok(()) => {
                let guard = EphemeralCheckout {
                    repo: self.target.clone(),
                    path: ephemeral.clone(),
                };
                (ephemeral, true, Some(guard))
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "reviewer checkout: ephemeral recreate failed; falling back to target (checks will spawn-fail)"
                );
                (self.target.clone(), false, None)
            }
        }
    }

    /// Create an ephemeral, detached worktree at `path` checked out at
    /// `reference`. Prunes stale registrations first so a crashed worktree's
    /// leftover metadata doesn't block `git worktree add`.
    async fn create_ephemeral_checkout(&self, path: &std::path::Path, reference: &str) -> Result<(), String> {
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.target)
            .args(["worktree", "prune"])
            .output()
            .await;
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.target)
            .args(["worktree", "add", "--detach"])
            .arg(path)
            .arg(reference)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(())
    }

    /// Wake the per-Plan Director task so it reacts to a Director-actionable
    /// state change (Bundle Reviewed/Rejected, Work Blocked/Ready)
    /// immediately instead of waiting out its idle poll. A missing entry is
    /// benign — the same contract `handle_director_chat` relies on; the next
    /// poll picks the change up regardless, so this is a latency
    /// optimization, never a correctness dependency. MUST be called AFTER the
    /// triggering transition's persist completes so the woken Director reads
    /// fresh on-disk state. `notify_one` collapses multiple pre-wait wakes
    /// into one, so this never busy-loops the Director.
    pub async fn wake_director(&self, plan_id: &domain::PlanId) {
        if let Some(notify) = self.operator_notifies.read().await.get(plan_id) {
            notify.notify_one();
        }
    }

    pub fn tool_context(&self, work_id: &WorkId, invocation_id: Uuid) -> ToolContext {
        let persist_base = telemetry::session_run_dir(&self.session_id, &self.target_slug, &self.process_id)
            .ok()
            .map(|d| d.join("work").join(work_id.as_ref()));
        ToolContext {
            working_dir: self.target.clone(),
            router: self.router.clone(),
            sandbox: self.sandbox,
            path_deny_patterns: self.path_deny_patterns.clone(),
            bash_denylist: self.bash_denylist.clone(),
            persist_base,
            invocation_id: Some(invocation_id),
        }
    }
}

/// RAII guard that reaps an ephemeral review worktree on drop. Best-effort
/// synchronous removal, mirroring `Worktree::Drop`'s safety-net posture: a
/// failed cleanup logs and defers to the startup reconcile sweep. Held in
/// `spawn_reviewer_for_bundle` for the duration of the review so every return
/// path (verdict, error, panic-unwind) reaps the ephemeral checkout.
struct EphemeralCheckout {
    repo: PathBuf,
    path: PathBuf,
}

impl Drop for EphemeralCheckout {
    fn drop(&mut self) {
        if let Err(e) = worktree::cleanup_at(&self.repo, &self.path) {
            warn!(
                path = %self.path.display(),
                error = %e,
                "ephemeral review worktree cleanup failed (reconcile will sweep)"
            );
        }
    }
}

/// Resolve the current HEAD commit of the target repo. Async via
/// `tokio::process::Command` so git subprocess spawning doesn't
/// block the tokio reactor.
async fn rev_parse_head(target: &std::path::Path) -> Result<String, std::io::Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(target)
        .args(["rev-parse", "HEAD"])
        .output()
        .await?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// The FSM-transition-and-persist helpers (~415 lines) and the sibling-Work
// reaction helpers (~140 lines) live in `transition.rs` / `siblings.rs` next
// door so this file stays under the per-file line limit (same pattern as
// `spawner.rs` / `integration.rs` / `reap.rs`). Re-exported here so every
// existing `crate::daemon::context::X` / `super::X` import path (integration,
// spawner, transport/handler, startup, integration tests) keeps resolving.
mod transition;
pub(crate) use transition::compute_plan_summary_extras;
pub use transition::{
    BundleTransitionError, MAX_WORK_ATTEMPTS_HARD_CAP, PlanSummaryExtras, TransitionError,
    discriminate_stale_bundle_write, transition_and_persist_bundle, transition_and_persist_plan,
    transition_and_persist_work,
};

mod siblings;
pub(crate) use siblings::{block_dependent_siblings, promote_unblocked_siblings};

// `DaemonSpawner` and the `WorkSpawner` impl that consumed ~330 lines
// here live in `spawner.rs` next door so this file stays under the
// per-file line limit. Re-exported below so external call sites
// (`transport/handler.rs`, `daemon/startup.rs`) keep their existing
// import paths.
mod spawner;
pub use spawner::DaemonSpawner;

// `spawn_integrator_for_bundle` (~220 lines) lives in `integration.rs`
// next door — an inherent-impl method on `DaemonContext` in a child
// module — so this file stays under the per-file line limit (same pattern
// as `spawner.rs`). Named `integration`, not `integrator`, to avoid
// shadowing the external `integrator` crate inside this module.
mod integration;

// Phase 19 (verified-swarm): live worktree + branch reaping the instant a
// Work lands on a terminal status. Split out for the same per-file-limit
// reason as `spawner.rs` / `integration.rs`; re-exported so `integration.rs`
// and `spawner.rs` (its module siblings) can call it via `super::`.
mod reap;
pub(crate) use reap::reap_terminal_work_worktree;

#[cfg(test)]
mod tests;
