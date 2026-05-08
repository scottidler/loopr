//! `DaemonContext`: shared state for the daemon run.
//!
//! Held in an `Arc` by the accept loop, each connection-handler task, and
//! the signal-watcher task. Values are set once at startup and read-only
//! thereafter; the only mutable cell is `shutting_down`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{Mutex, Notify, broadcast};
use tokio::task::JoinSet;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use agents::{
    Deps, ImplementerConfig, ImplementerError, RealTools, ReviewerConfig, ReviewerDeps, ReviewerError, run_implementer,
    run_reviewer,
};
use context::{InlineContextBuilder, StateSummary};
use domain::{Bundle, BundleStatus, Plan, PlanId, PlanStatus, Role, Verdict, Work, WorkId, WorkStatus};
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
    /// Per-process counter snapshot. Held in a `std::sync::Mutex`
    /// because every emitter is short, non-async, and the value is
    /// shared with the panic hook and SIGQUIT handler (both of which
    /// run on threads that aren't part of the tokio reactor and
    /// can't .await). Phase 7 of the Tier-1 cleanup wires this in;
    /// the daemon writes a per-process digest at exit.
    pub snapshot: Arc<StdMutex<ProcessSnapshot>>,
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
        worktree_cleanup_policy: AttemptCleanupPolicy,
        snapshot: Arc<StdMutex<ProcessSnapshot>>,
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
            git_lock: Arc::new(Mutex::new(())),
            worktree_cleanup_policy,
            implementer_tasks: Mutex::new(JoinSet::new()),
            reviewer_tasks: Mutex::new(JoinSet::new()),
            integrator_tasks: Mutex::new(JoinSet::new()),
            snapshot,
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

        let deps = Deps {
            llm: Arc::clone(&self.llm),
            tools,
            bundles: &self.store,
            context: Arc::clone(&self.context_builder),
            config: self.implementer_config.clone(),
            tool_schemas,
            state: StateSummary::default(),
        };

        let result = run_implementer(&work, &worktree, &deps).await;
        match result {
            Ok(bundle) => {
                info!(bundle_id = %bundle.id, "implementer produced bundle");
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
            Err(ImplementerError::EscalationNeeded(reason)) => {
                warn!(%reason, "implementer escalated; marking Work Blocked");
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    false,
                )
                .await;
            }
            Err(other) => {
                error!(error = %other, "implementer error; marking Work Blocked");
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
                if let Err(e) = transition_and_persist_work(
                    &self.store,
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
        };

        // Step 5: single LLM turn (plus bounded parse-retry inside run_reviewer).
        let verdict = match run_reviewer(&bundle, &work, &deps).await {
            Ok(v) => v,
            Err(ReviewerError::EscalationNeeded(reason)) => {
                warn!(%reason, "reviewer escalated; marking Work Blocked");
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                )
                .await;
                return;
            }
            Err(other) => {
                error!(error = %other, "reviewer error; marking Work Blocked");
                let _ = transition_and_persist_work(
                    &*self.summary_fanout,
                    &mut work,
                    WorkStatus::Blocked,
                    Role::Reactor,
                    true,
                )
                .await;
                return;
            }
        };

        // Step 6: route Verdict.
        match verdict {
            Verdict::Accept { summary } => {
                info!(summary = %summary, "reviewer accepted bundle");
                // Re-read Bundle: run_reviewer transitioned it to Reviewed
                // internally (via OCC on a clone) so our local copy is
                // stale. Fresh read gives us the new updated_at snapshot.
                let mut reviewed_bundle = match self.store.bundles().get(&bundle.id).await {
                    Ok(b) => b,
                    Err(e) => {
                        error!(error = %e, "bundle re-read after Accept failed");
                        return;
                    }
                };
                let expected = reviewed_bundle.updated_at;
                if let Err(e) = reviewed_bundle.transition(BundleStatus::Accepted, Role::Reactor) {
                    error!(error = %e, "Reviewed -> Accepted transition rejected by FSM");
                    return;
                }
                if let Err(e) = self.store.bundles().update(reviewed_bundle.clone(), expected).await {
                    warn!(error = %e, "Reviewed -> Accepted OCC update failed");
                    return;
                }
                // Stage 8 Phase 3 handoff: spawn Integrator task for the
                // Accepted Bundle. Shutdown-guard so a signal arriving here
                // does not spawn a task that will never drain.
                if !self.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                    let integrator_ctx = Arc::clone(&self);
                    let mut its = self.integrator_tasks.lock().await;
                    its.spawn(integrator_ctx.spawn_integrator_for_bundle(reviewed_bundle));
                }
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
                let mut plan_mut = plan.clone();
                if let Ok(siblings) = self.store.works().list_by_parent_id(&plan_mut.id).await {
                    let all_terminal = !siblings.is_empty() && siblings.iter().all(|w| w.status.is_terminal());
                    let any_done = siblings.iter().any(|w| w.status == WorkStatus::Done);
                    if all_terminal && any_done {
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
        let pending: Vec<Work> = siblings
            .iter()
            .filter(|w| w.status == WorkStatus::Pending)
            .cloned()
            .collect();
        let mut promoted = 0usize;
        for work in pending {
            if work.all_deps_done(&siblings) {
                let mut tasks = ctx.implementer_tasks.lock().await;
                tasks.spawn(Arc::clone(&ctx).spawn_implementer_for_work(work));
                promoted += 1;
            }
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
        let pending_dependents: Vec<Work> = siblings
            .iter()
            .filter(|w| w.status == WorkStatus::Pending && w.dependencies.iter().any(|d| d == &terminal_work_id))
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

/// Transition a Work via the FSM (transition or override) and persist via
/// `WorksStore::update`. Returns Err if the FSM rejects the transition or
/// if persistence fails; the caller logs and decides whether to continue.
///
/// Stage 8 wiring capstone replaced the Stage 7 `mark_blocked` function,
/// which mutated `work.status = ...` by raw assignment and bypassed the
/// FSM entirely. Every Work-state change in the pipeline flows through
/// this helper.
pub(crate) async fn transition_and_persist_work<S>(
    sink: &S,
    work: &mut Work,
    target: WorkStatus,
    role: Role,
    override_: bool,
) -> Result<(), String>
where
    S: store::WorkUpdateSink,
{
    let expected_updated_at = work.updated_at;
    let result = if override_ {
        work.override_status(target, role)
            .map_err(|e| format!("fsm override rejected: {e}"))?
    } else {
        work.transition(target, role)
            .map_err(|e| format!("fsm transition rejected: {e}"))?
    };
    if result == domain::Transition::Unchanged {
        return Ok(());
    }
    sink.update(work.clone(), expected_updated_at)
        .await
        .map_err(|e| format!("works().update: {e}"))
}

/// Mirror of `transition_and_persist_work` for `Bundle` records.
/// Uses the OCC-aware `BundlesStore::update` with the pre-transition
/// `updated_at` as `expected_updated_at`. Consumed by
/// `sweep_bundles` to transition `Reviewed -> Accepted` during
/// crash-recovery (the Reviewer already fired that transition on the
/// happy path, so this helper exists for the reconcile path only).
pub(crate) async fn transition_and_persist_bundle<S>(
    sink: &S,
    bundle: &mut Bundle,
    target: BundleStatus,
    role: Role,
) -> Result<(), String>
where
    S: store::BundleUpdateSink,
{
    let expected = bundle.updated_at;
    let result = bundle
        .transition(target, role)
        .map_err(|e| format!("fsm transition rejected: {e}"))?;
    if result == domain::Transition::Unchanged {
        return Ok(());
    }
    sink.update(bundle.clone(), expected)
        .await
        .map_err(|e| format!("bundles().update: {e}"))
}

/// Mirror of `transition_and_persist_work` for `Plan` records. Consumed by
/// the Integrator spawn's `Active -> Complete` check once every sibling
/// Work is terminal.
///
/// Phase 6 widened the signature to take `children: Vec<Work>` per design
/// Alternatives §4 option (c-extended): the caller fetches children
/// before invoking this helper, and the sink (typically a
/// `SummaryFanout`) renders the Plan summary from `(plan, children)`.
pub(crate) async fn transition_and_persist_plan<S>(
    sink: &S,
    plan: &mut Plan,
    children: Vec<Work>,
    target: PlanStatus,
    role: Role,
) -> Result<(), String>
where
    S: store::PlanUpdateSink,
{
    let result = plan
        .transition(target, role)
        .map_err(|e| format!("fsm transition rejected: {e}"))?;
    if result == domain::Transition::Unchanged {
        return Ok(());
    }
    sink.update(plan.clone(), children)
        .await
        .map_err(|e| format!("plans().update: {e}"))
}
