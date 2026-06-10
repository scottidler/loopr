//! `DaemonContext`: shared state for the daemon run.
//!
//! Held in an `Arc` by the accept loop, each connection-handler task, and
//! the signal-watcher task. Values are set once at startup and read-only
//! thereafter; the only mutable cell is `shutting_down`.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock as StdRwLock;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{Mutex, Notify, RwLock, broadcast};
use tokio::task::JoinSet;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use agents::{
    Deps, DirectorConfig, DirectorStatusSnapshot, ImplementerConfig, ImplementerError, RealTools, ReviewerConfig,
    ReviewerDeps, ReviewerError, run_implementer, run_reviewer,
};
use context::{InlineContextBuilder, StateSummary};
use domain::{
    Bundle, BundleId, BundleStatus, FailureReason, Plan, PlanId, PlanStatus, Role, Verdict, Work, WorkGraph, WorkId,
    WorkStatus,
};
use futures_util::FutureExt;
// Stage 8 used to consume `BundleUpdateError` here; Director Phase 3
// shifts that match into the `WorkSpawner::accept_bundle` path which
// matches `StoreError::Stale` directly. The import is kept available for
// callers/sinks even though it's no longer named in this module.
use integrator::{IntegrationError, IntegratorConfig, IntegratorDeps, integrate};
use ipc::DaemonEvent;
use llm::LlmClient;
use store::{BundleUpdateError, Store};
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

/// Capacity of the daemon's event broadcast channel. Stage 4 never sends
/// on it; the capacity is future-proofing for Stage 7+. v4 value.
pub const EVENTS_CAPACITY: usize = 64;

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
    ) -> Self {
        let (events, _) = broadcast::channel(EVENTS_CAPACITY);
        let store = Arc::new(store);
        let summary_fanout = Arc::new(crate::daemon::summary_fanout::SummaryFanout::new(
            Arc::clone(&store),
            target.clone(),
            Arc::clone(&store),
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
            reviewer_bundle_ids: Arc::new(StdRwLock::new(HashMap::new())),
            integrator_bundle_ids: Arc::new(StdRwLock::new(HashMap::new())),
            operator_notifies: Arc::new(RwLock::new(HashMap::new())),
            director_statuses: Arc::new(StdRwLock::new(HashMap::new())),
            snapshot,
            server_timeouts,
        }
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
    #[tracing::instrument(level = "info", skip_all, fields(work_id = %work.id, session_id = %self.session_id))]
    #[instrument(
        name = "daemon.spawn_implementer_for_work",
        level = "info",
        skip_all,
        fields(work_id = %work.id, work_status = ?work.status),
    )]
    pub async fn spawn_implementer_for_work(self: Arc<Self>, mut work: Work) {
        // Phase 2 sidecar-map insert. Guard is dropped on every exit
        // (panic or normal return), so `WorkSpawner::list_running_work_ids`
        // observes the live set even across abrupt panics.
        let _id_guard = ScopedIdGuard::new(Arc::clone(&self.implementer_work_ids), work.id.clone());

        // Advance Work through the pipeline-start transitions via the FSM.
        // Guarded: reconcile or a prior call may have advanced us already.
        if work.status == WorkStatus::Pending
            && let Err(e) = transition_and_persist_work(
                &*self.summary_fanout,
                &mut work,
                WorkStatus::Ready,
                Role::Reactor,
                false,
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
        // verification into the retry Implementer's StateSummary so it
        // learns WHY the prior bundle was rejected. Pre-fix this was
        // hardcoded `StateSummary::default()`, severing the doom-loop
        // feedback channel that exists end-to-end except this one wire.
        let rejected_bundle_reason = match self.store.bundles().list_by_work_id(&work.id).await {
            Ok(bundles) => bundles
                .into_iter()
                .filter(|b| b.status == BundleStatus::Rejected)
                .max_by_key(|b| b.updated_at)
                .map(|b| b.verification)
                .filter(|v| !v.trim().is_empty()),
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
        let result = std::panic::AssertUnwindSafe(run_implementer(&work, &worktree, &deps))
            .catch_unwind()
            .await;
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
                )
                .await
                {
                    error!(error = %e, "InProgress -> InReview transition failed after successful implementer");
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
    #[tracing::instrument(level = "info", skip_all, fields(bundle_id = %bundle.id, work_id = %bundle.work_id, session_id = %self.session_id))]
    #[instrument(
        name = "daemon.spawn_reviewer_for_bundle",
        level = "info",
        skip_all,
        fields(bundle_id = %bundle.id, work_id = %bundle.work_id, bundle_status = ?bundle.status),
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
        // returns Stale and we exit cleanly.
        let expected = bundle.updated_at;
        if let Err(e) = bundle.transition(BundleStatus::Triaged, Role::Reactor) {
            error!(error = %e, "bundle Proposed -> Triaged transition rejected by FSM; skipping");
            return;
        }
        if let Err(e) = self.store.bundles().update(bundle.clone(), expected).await {
            warn!(error = %e, "triage OCC update failed (another task beat us?); skipping");
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
                let expected = bundle.updated_at;
                bundle.failure_reason = Some(FailureReason::Other("work blocked; bundle superseded".to_string()));
                if let Err(e) = bundle.transition(BundleStatus::Superseded, Role::Reactor) {
                    error!(error = %e, "Bundle -> Superseded rejected by FSM; skipping");
                    return;
                }
                if let Err(e) = self.store.bundles().update(bundle.clone(), expected).await {
                    warn!(error = %e, "Bundle supersede OCC update failed (another task beat us?); skipping");
                }
                return;
            }
            other => {
                warn!(?other, "unexpected Work status at reviewer entry; skipping");
                return;
            }
        }

        // Step 4: build ReviewerDeps.
        // Phase 6: pass the SummaryFanout decorator as the BundleUpdateSink
        // so per-Bundle summaries land transactionally with the OCC
        // update; the inner sink is `Arc<Store>` and the decorator's
        // BundleUpdateSink impl writes the summary on Ok.
        let deps = ReviewerDeps {
            llm: Arc::clone(&self.llm),
            store: &*self.summary_fanout,
            context: Arc::clone(&self.context_builder),
            config: self.reviewer_config.clone(),
            target: self.target.clone(),
            path_deny_patterns: self.path_deny_patterns.clone(),
        };

        // Step 5: single LLM turn (plus bounded parse-retry inside run_reviewer).
        // Panic posture: `catch_unwind` so a panic inside `run_reviewer`
        // records `FailureReason::Panic`, Blocks the Work, and wakes the
        // Director instead of silently killing this reviewer task (the
        // JoinSet would otherwise swallow it). The Bundle stays Triaged
        // and is superseded by the next recovery sweep's Work-Blocked
        // entry guard.
        let reviewer_result = std::panic::AssertUnwindSafe(run_reviewer(&bundle, &work, &deps))
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
                )
                .await;
                self.wake_director(&work.parent_id).await;
                return;
            }
            Ok(Ok(v)) => v,
            Ok(Err(ReviewerError::EscalationNeeded(reason))) => {
                warn!(%reason, "reviewer escalated; marking Work Blocked");
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                )
                .await;
                self.wake_director(&work.parent_id).await;
                return;
            }
            // F6: a benign OCC lost-race (the winning Reviewer already
            // persisted this Bundle's verdict) must NOT force the Work to
            // Blocked — that manufactures divergence (Bundle Reviewed while
            // Work Blocked). Drop the losing verdict silently and let the
            // winner's routing stand. Mirrors spawner.rs's accept_bundle
            // Stale handling.
            Ok(Err(ReviewerError::Update(store::BundleUpdateError::Stale { .. }))) => {
                debug!("reviewer OCC Stale; another reviewer won, leaving Work untouched");
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

    /// Integrate an Accepted Bundle onto the Plan's integration branch
    /// and produce a Tick. Retries on transient errors with the
    /// `INTEGRATOR_BACKOFF` schedule, capped at 5 attempts total.
    ///
    /// Integrator doc contract: a Bundle at `Integrating` on `integrate`
    /// return is NOT a terminal failure; the daemon re-enqueues it.
    /// This method honors that by treating `Update(Stale)` and `Store`
    /// errors as retryable, and any other IntegrationError variant as
    /// terminal (no retry; Work -> Blocked).
    ///
    /// Shutdown-aware: shutdown_notify cuts the backoff sleep so a Ctrl-C
    /// during a retry does not block the daemon for 12.6s.
    #[tracing::instrument(level = "info", skip_all, fields(bundle_id = %bundle.id, work_id = %bundle.work_id, session_id = %self.session_id))]
    #[instrument(
        name = "daemon.spawn_integrator_for_bundle",
        level = "info",
        skip_all,
        fields(bundle_id = %bundle.id, work_id = %bundle.work_id),
    )]
    pub async fn spawn_integrator_for_bundle(self: Arc<Self>, bundle: Bundle) {
        // Phase 2 sidecar-map insert; mirrors the implementer/reviewer wrappers.
        let _id_guard = ScopedIdGuard::new(Arc::clone(&self.integrator_bundle_ids), bundle.id.clone());

        if self.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
            debug!("shutdown in progress; skipping integrator spawn");
            return;
        }

        // Load Work + Plan. Both failures are non-retryable (records
        // fundamentally missing), so we log and return.
        let mut work = match self.store.works().get(&bundle.work_id).await {
            Ok(w) => w,
            Err(e) => {
                error!(error = %e, "work lookup failed during integrate; skipping");
                return;
            }
        };
        let plan = match self.store.plans().get(&work.parent_id).await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "plan lookup failed during integrate; skipping");
                return;
            }
        };

        let deps = IntegratorDeps {
            // Phase 6: BundleUpdateSink goes through the fanout so the
            // Integrator's `Reviewed -> Merged` write produces an
            // up-to-date Bundle summary in lockstep with the OCC
            // update. `works` and `ticks` are read paths (resolved on
            // `Store` directly) and stay on the underlying store.
            bundle_sink: &*self.summary_fanout,
            works: &*self.store,
            ticks: &*self.store,
            config: self.integrator_config.clone(),
            target: self.target.clone(),
            git_lock: Arc::clone(&self.git_lock),
        };

        // Retry loop with circuit breaker. `attempt` is 0-indexed into
        // INTEGRATOR_BACKOFF; each iteration either integrates or sleeps
        // the corresponding backoff then tries again.
        let outcome: Result<domain::Tick, IntegrationError> = 'retry: {
            for (attempt, &backoff) in INTEGRATOR_BACKOFF.iter().enumerate() {
                match integrate(std::slice::from_ref(&bundle), &plan, &deps).await {
                    Ok(tick) => break 'retry Ok(tick),
                    Err(IntegrationError::Update(BundleUpdateError::Stale { .. }))
                    | Err(IntegrationError::Store(_))
                        if attempt + 1 < INTEGRATOR_BACKOFF.len() =>
                    {
                        warn!(
                            attempt = attempt + 1,
                            total_attempts = INTEGRATOR_BACKOFF.len(),
                            backoff_ms = backoff.as_millis(),
                            "integrator retryable error; backing off"
                        );
                        // Respect shutdown during backoff; select against
                        // the notify waker so a SIGTERM does not block.
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            _ = self.shutdown_notify.notified() => {
                                warn!("shutdown during integrator backoff; abandoning retry");
                                return;
                            }
                        }
                    }
                    Err(e) => break 'retry Err(e),
                }
            }
            // Fell off the end of the schedule with all attempts retryable.
            // Circuit-break.
            Err(IntegrationError::Git(
                "integrator circuit breaker tripped: 5 retryable-error attempts exhausted".into(),
            ))
        };

        match outcome {
            Ok(tick) => {
                info!(tick_id = %tick.id, sha = %tick.sha, "integration succeeded");
                // Work: InReview -> Integrated -> Done.
                if let Err(e) = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Integrated,
                    Role::Integrator,
                    false,
                )
                .await
                {
                    error!(error = %e, "InReview -> Integrated transition failed after Tick persisted");
                    return;
                }
                if let Err(e) = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Done,
                    Role::Reactor,
                    false,
                )
                .await
                {
                    error!(error = %e, "Integrated -> Done transition failed after Tick persisted");
                    return;
                }
                // Dep gate: promote any Pending siblings whose deps are
                // now all Done. Best-effort; failure is already logged
                // inside promote_unblocked_siblings.
                {
                    let ctx = Arc::clone(&self);
                    promote_unblocked_siblings(ctx, plan.id.clone()).await;
                }

                // Plan-level completion check: if every sibling Work is
                // terminal with at least one Done, fire Plan:
                // Active -> Complete. Best-effort; log + continue on Err.
                //
                // F1: re-fetch the Plan fresh immediately before the
                // transition rather than reusing the snapshot loaded at
                // the top of this method (minutes ago, before the
                // integrate). A concurrent Director/IPC write since then
                // would otherwise make the OCC `expected_updated_at`
                // stale and reject the Complete transition spuriously.
                let mut plan_mut = match self.store.plans().get(&plan.id).await {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(error = %e, "plan re-fetch before completion check failed; skipping");
                        return;
                    }
                };
                if let Ok(siblings) = self.store.works().list_by_parent_id(&plan_mut.id).await {
                    let all_terminal = !siblings.is_empty() && siblings.iter().all(|w| w.status.is_terminal());
                    let any_done = siblings.iter().any(|w| w.status == WorkStatus::Done);
                    if all_terminal && any_done {
                        // Phase 8: compute Plan-level summary extras
                        // (ticks + bundle terminal counts) from the
                        // store. Best-effort: a query failure leaves
                        // the field at 0 rather than failing the
                        // Plan transition.
                        let extras = compute_plan_summary_extras(&self.store, &plan_mut.id, &siblings).await;
                        // Phase 6: c-extended (option c) — pass siblings as
                        // the children arg so SummaryFanout's PlanUpdateSink
                        // impl can render the Plan summary against the
                        // current child set without a separate read.
                        match transition_and_persist_plan(
                            &*self.summary_fanout,
                            &mut plan_mut,
                            siblings,
                            PlanStatus::Complete,
                            Role::Reactor,
                            extras,
                            false,
                        )
                        .await
                        {
                            Ok(()) => info!(plan_id = %plan_mut.id, "plan Active -> Complete"),
                            Err(e) => warn!(error = %e, "plan Active -> Complete transition failed (non-fatal)"),
                        }
                    }
                }
                // Per-record summaries are now written transactionally
                // by SummaryFanout inside each transition's `update`
                // call. The post-Integrator inline `write_*_summary_best_effort`
                // helpers (and the post-fetch reads used to build them)
                // are gone — kept as a comment for the historical record.
                let _ = bundle; // bundle was previously re-fetched here for the inline summary
                let _ = work;
            }
            Err(IntegrationError::ValidationFailed {
                ref command, exit_code, ..
            }) => {
                // Bundles are already IntegrationFailed (integrate() called
                // fail_all_without_reset before returning). Only Work needs
                // a state change here.
                warn!(
                    command = %command,
                    exit_code = ?exit_code,
                    "post-merge validation failed; marking Work Blocked"
                );
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                )
                .await;
                self.wake_director(&work.parent_id).await;
            }
            Err(e) => {
                error!(error = %e, "integrator terminal; marking Work Blocked");
                // One-step via the Phase 1 InReview -> Blocked override.
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                )
                .await;
                self.wake_director(&work.parent_id).await;
            }
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

/// Scan Pending sibling Works for the given Plan and spawn an
/// Implementer for any whose deps are now all Done.
///
/// Returns `Pin<Box<dyn Future<...>>>` (not `impl Future`) so rustc can
/// resolve the return type without following the async call graph into
/// `spawn_implementer_for_work` -> `spawn_reviewer_for_bundle` ->
/// `spawn_integrator_for_bundle` -> this function (E0391 cycle). A
/// concrete boxed-future return type breaks the opaque-type cycle at
/// this edge.
///
/// Called after every `Integrated -> Done` transition and during
/// startup reconcile (crash-recovery gap). Best-effort: store errors
/// are logged and dropped so a sibling-sweep failure never kills the
/// caller's success path.
pub(crate) fn promote_unblocked_siblings<L: LlmClient + Send + Sync + 'static>(
    ctx: Arc<DaemonContext<L>>,
    plan_id: PlanId,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
    Box::pin(async move {
        let span = tracing::info_span!("daemon.promote_unblocked_siblings", plan_id = %plan_id);
        let _enter = span.enter();
        let siblings = match ctx.store.works().list_by_parent_id(&plan_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "promote_unblocked_siblings: list_by_parent_id failed");
                return;
            }
        };
        let graph = WorkGraph::from_works(&siblings);
        let done: HashSet<WorkId> = siblings
            .iter()
            .filter(|w| w.status == WorkStatus::Done)
            .map(|w| w.id.clone())
            .collect();
        let ready: HashSet<WorkId> = graph.ready_set(&done).into_iter().collect();
        let pending_ready: Vec<Work> = siblings
            .into_iter()
            .filter(|w| w.status == WorkStatus::Pending && ready.contains(&w.id))
            .collect();
        let promoted = pending_ready.len();
        for work in pending_ready {
            let mut tasks = ctx.implementer_tasks.lock().await;
            tasks.spawn(Arc::clone(&ctx).spawn_implementer_for_work(work));
        }
        info!(promoted, "promote_unblocked_siblings: done");
    })
}

/// Mark any Pending Works whose `dependencies` contains `terminal_work_id`
/// as `Blocked`, writing `blocked_reason` to explain that a dep became
/// irrecoverable.
///
/// Only called when `terminal_work_id` reaches `Abandoned` or `Superseded`
/// (truly terminal, non-Done). `Blocked` deps are excluded because they
/// may still recover via 1.3's recovery loop.
///
/// Returns `Pin<Box<dyn Future<...>>>` for the same E0391 reason as
/// `promote_unblocked_siblings`.
pub(crate) fn block_dependent_siblings<L: LlmClient + Send + Sync + 'static>(
    ctx: Arc<DaemonContext<L>>,
    plan_id: PlanId,
    terminal_work_id: WorkId,
    terminal_status: WorkStatus,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
    Box::pin(async move {
        let span = tracing::warn_span!(
            "daemon.block_dependent_siblings",
            plan_id = %plan_id,
            terminal_work_id = %terminal_work_id,
            terminal_status = ?terminal_status,
        );
        let _enter = span.enter();
        let siblings = match ctx.store.works().list_by_parent_id(&plan_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "block_dependent_siblings: list_by_parent_id failed");
                return;
            }
        };
        let graph = WorkGraph::from_works(&siblings);
        // F7: block the FULL transitive closure of dependents, not just
        // the direct ones. A Work that depends (even transitively) on a
        // terminal Work can never have all its deps reach Done, so it is
        // irrecoverable. BFS over the reverse-dependency edges from the
        // terminal Work to a fixpoint; the graph topology is static, so a
        // single closure pass IS the fixpoint (no re-listing needed).
        let mut closure: HashSet<WorkId> = HashSet::new();
        let mut frontier: Vec<WorkId> = graph.dependents_of(&terminal_work_id).to_vec();
        while let Some(node) = frontier.pop() {
            if closure.insert(node.clone()) {
                frontier.extend(graph.dependents_of(&node).iter().cloned());
            }
        }
        let pending_dependents: Vec<Work> = siblings
            .iter()
            .filter(|w| w.status == WorkStatus::Pending && closure.contains(&w.id))
            .cloned()
            .collect();
        let mut blocked = 0usize;
        for mut work in pending_dependents {
            work.blocked_reason = Some(format!(
                "dep {} reached {:?}; irrecoverable",
                terminal_work_id, terminal_status
            ));
            if let Err(e) = transition_and_persist_work(
                &*ctx.summary_fanout,
                &mut work,
                WorkStatus::Blocked,
                Role::Reactor,
                false,
            )
            .await
            {
                warn!(work_id = %work.id, error = %e, "block_dependent_siblings: Pending -> Blocked failed");
            } else {
                blocked += 1;
            }
        }
        warn!(blocked, "block_dependent_siblings: done");
    })
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

/// Spawner-layer hard cap on `Work.attempt_count` — defense-in-depth
/// circuit breaker. The Director-layer soft cap
/// (`DirectorConfig.max_work_attempts`, default 3) is the well-behaved
/// exit path; this constant catches retry paths that bypass the soft cap
/// (rogue caller, manual CLI intervention, future bug) and push a Work
/// to Ready a hundred-plus times. Set far above any plausible
/// operator-tunable soft cap.
pub const MAX_WORK_ATTEMPTS_HARD_CAP: u32 = 100;

/// Typed failure of `transition_and_persist_work`. Replaces the prior
/// `String` so callers can distinguish a benign OCC lost-race (`Stale`)
/// from a hard failure — `override_work` must NOT spawn an Implementer
/// when the persist lost the race (the persisted state belongs to the
/// racing winner), and the reviewer/spawner paths treat `Stale` as a
/// no-op rather than forcing the Work to `Blocked`.
#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    /// The FSM rejected the (override) transition.
    #[error("fsm rejected: {0}")]
    Fsm(String),
    /// Layer-3 hard cap refused the persist (attempt_count at the cap).
    #[error("work {work_id} attempt_count={attempt_count} hit MAX_WORK_ATTEMPTS_HARD_CAP={cap}; refusing persist")]
    HardCap {
        work_id: String,
        attempt_count: u32,
        cap: u32,
    },
    /// OCC version-check lost the race — benign; the Work was already
    /// advanced by another writer. Callers should return without
    /// clobbering rather than treat it as a hard failure.
    #[error("stale work: expected updated_at={expected}, actual={actual}")]
    Stale { expected: i64, actual: i64 },
    /// Underlying store write failed (non-OCC).
    #[error("works().update: {0}")]
    Persist(String),
}

/// Transition a Work via the FSM (transition or override) and persist via
/// `WorksStore::update`. Returns a typed `TransitionError` if the FSM
/// rejects the transition or if persistence fails; the caller logs and
/// decides whether to continue (matching `Stale` for benign races).
///
/// Stage 8 wiring capstone replaced the Stage 7 `mark_blocked` function,
/// which mutated `work.status = ...` by raw assignment and bypassed the
/// FSM entirely. Every Work-state change in the pipeline flows through
/// this helper.
///
/// **Retry-budget instrumentation (Director Phase 1 follow-ups, Layers 1
/// and 3).** On any successful transition where the new status is `Ready`,
/// `work.attempt_count` increments by 1 BEFORE the persist write, so a
/// Work that has run once has `attempt_count == 1` (1-based). Layer 3's
/// hard cap pre-checks the count BEFORE the increment; the persist is
/// refused if `attempt_count >= MAX_WORK_ATTEMPTS_HARD_CAP`.
pub async fn transition_and_persist_work<S>(
    sink: &S,
    work: &mut Work,
    target: WorkStatus,
    role: Role,
    override_: bool,
) -> Result<(), TransitionError>
where
    S: store::WorkUpdateSink,
{
    let expected_updated_at = work.updated_at;
    let result = if override_ {
        work.override_status(target, role)
            .map_err(|e| TransitionError::Fsm(format!("override: {e}")))?
    } else {
        work.transition(target, role)
            .map_err(|e| TransitionError::Fsm(format!("transition: {e}")))?
    };
    if result == domain::Transition::Unchanged {
        return Ok(());
    }

    // Layer 3 hard cap: refuse persist when attempt_count is already at
    // the hard cap. Pre-increment (>=) check keeps the cap strict — the
    // current attempt would be the (HARD_CAP+1)th if it landed.
    //
    // Order note: the design doc's Phase 4 prose puts this check
    // *after* Layer 1's increment. Implementing in that order requires
    // a `>` comparison against `HARD_CAP+1` (the post-increment value)
    // to fire on the same attempt; the as-implemented "check before
    // increment with `>=`" is the same boundary expressed without the
    // off-by-one. Documented here so a future reader doesn't try to
    // "fix" it back to the spec's literal sequence.
    if matches!(target, WorkStatus::Ready) && work.attempt_count >= MAX_WORK_ATTEMPTS_HARD_CAP {
        return Err(TransitionError::HardCap {
            work_id: work.id.to_string(),
            attempt_count: work.attempt_count,
            cap: MAX_WORK_ATTEMPTS_HARD_CAP,
        });
    }

    // Layer 1 increment: bump the cross-iteration retry counter on any
    // path to Ready. Fires for both the initial Pending->Ready dispatch
    // (first attempt) and Director-issued Blocked->Ready retries.
    if matches!(target, WorkStatus::Ready) {
        work.attempt_count = work.attempt_count.saturating_add(1);
    }

    // Sync the in-memory Work to the persisted (monotonically-floored)
    // `updated_at` so a chained next transition on the same record (e.g.
    // Integrated -> Done in `spawn_integrator_for_bundle`) carries the
    // correct OCC expected-version even when both writes land in the same
    // millisecond.
    let persisted = match sink.update(work.clone(), expected_updated_at).await {
        Ok(ts) => ts,
        Err(store::WorkUpdateError::Stale { expected, actual }) => {
            return Err(TransitionError::Stale { expected, actual });
        }
        Err(store::WorkUpdateError::Update(s)) => return Err(TransitionError::Persist(s)),
    };
    work.updated_at = persisted;

    // Phase 8: per-Work terminal summary. The richer metrics
    // (total_iterations, lifeguard_fires, director_override_count) are
    // not yet aggregated daemon-side; this event opens the door so a
    // future commit can extend the field set without changing the
    // event name. For now: work_id + terminal_state + role + override
    // is enough to grep "every Work that reached terminal in this run."
    if work.status.is_terminal() {
        info!(
            work_id = %work.id,
            plan_id = %work.parent_id,
            terminal_state = ?work.status,
            role = ?role,
            override_,
            attempt_count = work.attempt_count,
            session_failure_count = work.session_failure_count,
            "work: terminal-state summary"
        );
    }
    Ok(())
}

/// Mirror of `transition_and_persist_work` for `Plan` records. Consumed by
/// the Integrator spawn's `Active -> Complete` check once every sibling
/// Work is terminal.
///
/// Phase 6 widened the signature to take `children: Vec<Work>` per design
/// Alternatives §4 option (c-extended): the caller fetches children
/// before invoking this helper, and the sink (typically a
/// `SummaryFanout`) renders the Plan summary from `(plan, children)`.
/// Extra Plan-level counts surfaced on the `plan: terminal-state
/// summary` event. Computed at the daemon call site from the store
/// (Tick / Bundle queries) so the helper itself doesn't need a store
/// handle. `Default::default()` is acceptable for tests; production
/// callers should populate from real queries.
#[derive(Default)]
pub struct PlanSummaryExtras {
    pub ticks: u64,
    pub bundles_accepted: u64,
    pub bundles_rejected: u64,
}

/// Compute `PlanSummaryExtras` for a finishing Plan. `ticks` is a
/// direct query; `bundles_accepted` / `bundles_rejected` walk the
/// Plan's child Works fanned out to each Work's Bundle list. A
/// `bundle.status` of `Reviewed` / `Accepted` / `Integrating` /
/// `Merged` counts as accepted; `Rejected` / `IntegrationFailed`
/// counts as rejected. Other statuses (Triaged, ProposedNoop, etc.)
/// are pre-decision and don't contribute to either count.
///
/// Best-effort: any individual store error is folded into the
/// running counter rather than aborting — a missing Bundle list for
/// one Work shouldn't block the Plan's terminal summary.
async fn compute_plan_summary_extras(store: &Store, plan_id: &PlanId, children: &[Work]) -> PlanSummaryExtras {
    let mut extras = PlanSummaryExtras::default();
    if let Ok(ticks) = store.ticks().list_by_plan_id(plan_id).await {
        extras.ticks = ticks.len() as u64;
    }
    for work in children {
        let Ok(bundles) = store.bundles().list_by_work_id(&work.id).await else {
            continue;
        };
        for b in bundles {
            match b.status {
                BundleStatus::Reviewed | BundleStatus::Accepted | BundleStatus::Integrating | BundleStatus::Merged => {
                    extras.bundles_accepted += 1
                }
                BundleStatus::Rejected | BundleStatus::IntegrationFailed => extras.bundles_rejected += 1,
                _ => {}
            }
        }
    }
    extras
}

pub async fn transition_and_persist_plan<S>(
    sink: &S,
    plan: &mut Plan,
    children: Vec<Work>,
    target: PlanStatus,
    role: Role,
    extras: PlanSummaryExtras,
    override_: bool,
) -> Result<(), String>
where
    S: store::PlanUpdateSink,
{
    // OCC snapshot BEFORE the FSM transition bumps `plan.updated_at`.
    let expected_updated_at = plan.updated_at;
    let result = if override_ {
        plan.override_status(target, role)
            .map_err(|e| format!("fsm override rejected: {e}"))?
    } else {
        plan.transition(target, role)
            .map_err(|e| format!("fsm transition rejected: {e}"))?
    };
    if result == domain::Transition::Unchanged {
        return Ok(());
    }

    // Snapshot the per-state Work counts BEFORE the sink moves
    // `children` into its `update` call.
    let (works_done, works_failed, works_blocked) = if plan.status.is_terminal() {
        let mut done = 0u64;
        let mut failed = 0u64;
        let mut blocked = 0u64;
        for w in &children {
            match w.status {
                WorkStatus::Done | WorkStatus::Integrated => done += 1,
                WorkStatus::Abandoned | WorkStatus::Superseded => failed += 1,
                WorkStatus::Blocked => blocked += 1,
                _ => {}
            }
        }
        (Some(done), Some(failed), Some(blocked))
    } else {
        (None, None, None)
    };
    let total_works = if plan.status.is_terminal() { Some(children.len() as u64) } else { None };
    let plan_terminal = plan.status.is_terminal();
    let plan_id = plan.id.clone();
    let plan_status = plan.status;

    let persisted = sink
        .update(plan.clone(), children, expected_updated_at)
        .await
        .map_err(|e| format!("plans().update: {e}"))?;
    plan.updated_at = persisted;

    // Phase 8: per-Plan terminal summary. `ticks`, `bundles_accepted`,
    // `bundles_rejected` come from the daemon-side `extras`
    // (computed from store queries at the call site, where a store
    // handle is available). `total_input_tokens` / `total_output_tokens`
    // / `total_cost_usd` are deferred — those numbers live on
    // `ProcessSnapshot` / `MeteredLlmClient` and would require
    // threading a snapshot handle in. Tracked as Open Question on
    // `docs/design/2026-05-09-comprehensive-telemetry.md`.
    if plan_terminal {
        info!(
            plan_id = %plan_id,
            terminal_state = ?plan_status,
            role = ?role,
            total_works = total_works.unwrap_or(0),
            works_done = works_done.unwrap_or(0),
            works_failed = works_failed.unwrap_or(0),
            works_blocked = works_blocked.unwrap_or(0),
            ticks = extras.ticks,
            bundles_accepted = extras.bundles_accepted,
            bundles_rejected = extras.bundles_rejected,
            "plan: terminal-state summary"
        );
    }
    Ok(())
}

// `DaemonSpawner` and the `WorkSpawner` impl that consumed ~330 lines
// here live in `spawner.rs` next door so this file stays under the
// per-file line limit. Re-exported below so external call sites
// (`transport/handler.rs`, `daemon/startup.rs`) keep their existing
// import paths.
mod spawner;
pub use spawner::DaemonSpawner;

#[cfg(test)]
mod tests;
