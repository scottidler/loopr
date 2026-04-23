//! `DaemonContext`: shared state for the daemon run.
//!
//! Held in an `Arc` by the accept loop, each connection-handler task, and
//! the signal-watcher task. Values are set once at startup and read-only
//! thereafter; the only mutable cell is `shutting_down`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::process::Command;
use tokio::sync::{Mutex, Notify, broadcast};
use tokio::task::JoinSet;
use tracing::{error, info, warn};
use uuid::Uuid;

use agents::{Deps, ImplementerConfig, ImplementerError, RealTools, run_implementer};
use context::{InlineContextBuilder, StateSummary};
use domain::{Plan, PlanStatus, Role, Work, WorkId, WorkStatus};
use ipc::DaemonEvent;
use llm::AnthropicClient;
use store::Store;
use telemetry::RunId;
use tools::{BashDenylist, LaneRouter, SandboxMode, ToolContext};
use worktree::{AttemptCleanupPolicy, Worktree};

/// Capacity of the daemon's event broadcast channel. Stage 4 never sends
/// on it; the capacity is future-proofing for Stage 7+. v4 value.
pub const EVENTS_CAPACITY: usize = 64;

pub struct DaemonContext {
    pub target: PathBuf,
    pub run_id: RunId,
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
    pub store: Store,
    /// Handle to the process-wide Anthropic client. Built once at
    /// daemon startup from `LlmConfig` + env-resolved API key and
    /// shared across handler tasks via `Arc`. Decompose call sites
    /// pass `&*ctx.llm` as the `&L` where `L: LlmClient`; the Arc
    /// deref produces `&AnthropicClient`, which implements the trait.
    pub llm: Arc<AnthropicClient>,
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
}

impl DaemonContext {
    /// Construct a new context. All fields are set once at daemon startup;
    /// nothing mutable is exposed except the `shutting_down` atomic.
    /// Takes an already-opened `Store`, a pre-built `AnthropicClient`, and
    /// already-built tool infrastructure (`LaneRouter`, `BashDenylist`,
    /// `path_deny_patterns`, `SandboxMode`). Router construction can fail
    /// when `SandboxMode::Required` + no bwrap; that failure happens in
    /// `run_active_daemon` before this constructor, so this function is
    /// infallible.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: PathBuf,
        run_id: RunId,
        pid: u32,
        store: Store,
        llm: Arc<AnthropicClient>,
        router: Arc<LaneRouter>,
        bash_denylist: Arc<BashDenylist>,
        path_deny_patterns: Vec<String>,
        sandbox: SandboxMode,
        context_builder: Arc<InlineContextBuilder>,
        implementer_config: ImplementerConfig,
        worktree_cleanup_policy: AttemptCleanupPolicy,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENTS_CAPACITY);
        Self {
            target,
            run_id,
            started_at: chrono::Local::now(),
            pid,
            events,
            shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
            store,
            llm,
            router,
            bash_denylist,
            path_deny_patterns,
            sandbox,
            context_builder,
            implementer_config,
            worktree_cleanup_policy,
            implementer_tasks: Mutex::new(JoinSet::new()),
        }
    }

    /// Build a per-invocation `ToolContext` for one tool call.
    ///
    /// The persist base for overflow output is
    /// `<target>/.loopr/runs/<run-id>/work/<work-id>/`. The ralph loop
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
    #[tracing::instrument(level = "info", skip_all, fields(work_id = %work.id, run_id = %self.run_id))]
    pub async fn spawn_implementer_for_work(self: Arc<Self>, mut work: Work) {
        // Advance Work through the pipeline-start transitions via the FSM.
        // Guarded: reconcile or a prior call may have advanced us already.
        if work.status == WorkStatus::Pending
            && let Err(e) =
                transition_and_persist_work(&self.store, &mut work, WorkStatus::Ready, Role::Coordinator, false).await
        {
            error!(error = %e, "Pending -> Ready transition failed; abandoning task");
            return;
        }
        if work.status == WorkStatus::Ready
            && let Err(e) =
                transition_and_persist_work(&self.store, &mut work, WorkStatus::InProgress, Role::Coordinator, false)
                    .await
        {
            error!(error = %e, "Ready -> InProgress transition failed; abandoning task");
            return;
        }

        let base_sha = match rev_parse_head(&self.target).await {
            Ok(sha) => sha,
            Err(e) => {
                error!(error = %e, "base_sha lookup failed; Work remains InProgress");
                return;
            }
        };

        let worktree_root = self.target.join(".loopr").join("worktrees");
        let persist_base = self
            .target
            .join(".loopr")
            .join("runs")
            .join(self.run_id.as_str())
            .join("work")
            .join(work.id.as_ref());
        let _ = std::fs::create_dir_all(&persist_base);

        let target = self.target.clone();
        let root = worktree_root.clone();
        let wid = work.id.clone();
        let base = base_sha.clone();
        let worktree = match tokio::task::spawn_blocking(move || Worktree::create(&target, &root, wid, &base)).await {
            Ok(Ok(wt)) => wt,
            Ok(Err(e)) => {
                error!(error = %e, "Worktree::create failed");
                let _ =
                    transition_and_persist_work(&self.store, &mut work, WorkStatus::Blocked, Role::Coordinator, false)
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
                if let Err(e) =
                    transition_and_persist_work(&self.store, &mut work, WorkStatus::InReview, Role::Implementer, false)
                        .await
                {
                    error!(error = %e, "InProgress -> InReview transition failed after successful implementer");
                }
                // Phase 2 will spawn a Reviewer task here. For now the
                // Bundle is persisted and the Work is InReview.
            }
            Err(ImplementerError::EscalationNeeded(reason)) => {
                warn!(%reason, "implementer escalated; marking Work Blocked");
                let _ =
                    transition_and_persist_work(&self.store, &mut work, WorkStatus::Blocked, Role::Coordinator, false)
                        .await;
            }
            Err(other) => {
                error!(error = %other, "implementer error; marking Work Blocked");
                let _ =
                    transition_and_persist_work(&self.store, &mut work, WorkStatus::Blocked, Role::Coordinator, false)
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

    pub fn tool_context(&self, work_id: &WorkId, invocation_id: Uuid) -> ToolContext {
        let persist_base = self
            .target
            .join(".loopr")
            .join("runs")
            .join(self.run_id.as_str())
            .join("work")
            .join(work_id.as_ref());
        ToolContext {
            working_dir: self.target.clone(),
            router: self.router.clone(),
            sandbox: self.sandbox,
            path_deny_patterns: self.path_deny_patterns.clone(),
            bash_denylist: self.bash_denylist.clone(),
            persist_base: Some(persist_base),
            invocation_id: Some(invocation_id),
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

/// Transition a Work via the FSM (transition or override) and persist via
/// `WorksStore::update`. Returns Err if the FSM rejects the transition or
/// if persistence fails; the caller logs and decides whether to continue.
///
/// Stage 8 wiring capstone replaced the Stage 7 `mark_blocked` function,
/// which mutated `work.status = ...` by raw assignment and bypassed the
/// FSM entirely. Every Work-state change in the pipeline flows through
/// this helper.
pub(crate) async fn transition_and_persist_work(
    store: &Store,
    work: &mut Work,
    target: WorkStatus,
    role: Role,
    override_: bool,
) -> Result<(), String> {
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
    store
        .works()
        .update(work.clone())
        .await
        .map_err(|e| format!("works().update: {e}"))
}

/// Mirror of `transition_and_persist_work` for `Plan` records. Consumed by
/// the Integrator spawn's `Active -> Complete` check once every sibling
/// Work is terminal.
#[allow(dead_code)]
pub(crate) async fn transition_and_persist_plan(
    store: &Store,
    plan: &mut Plan,
    target: PlanStatus,
    role: Role,
) -> Result<(), String> {
    let result = plan
        .transition(target, role)
        .map_err(|e| format!("fsm transition rejected: {e}"))?;
    if result == domain::Transition::Unchanged {
        return Ok(());
    }
    store
        .plans()
        .update(plan.clone())
        .await
        .map_err(|e| format!("plans().update: {e}"))
}
