//! Daemon process lifecycle: double-fork detachment, pid/version/process-id
//! sentinels, signal handling, run loop. Owned by the `loopr` driver crate;
//! the `transport` module hangs off the daemon's accept loop but does not
//! know about the fork or the pid file.
//!
//! Submodules: `fork` (libc double-fork primitive), `sentinel` (pid /
//! version / process-id / socket filesystem helpers), `context` (`DaemonContext`
//! shared state).
//!
//! ## Fork control flow
//!
//! `ensure_daemon` / `ensure_daemon_if_needed` are the parent-side entry
//! points. The parent must not hold a tokio runtime; `lib::run` calls
//! these BEFORE its telemetry init so the grandchild inherits no
//! already-installed `tracing` subscriber.
//!
//! The grandchild never returns from `ensure_daemon`: it enters
//! `run_grandchild`, calls `daemon_main`, and then `process::exit`s.
//! Unwinding the stack back into `lib::run` would fall into the parent's
//! telemetry-init block and crash on `AlreadyInitialized`.

pub(crate) mod context;
pub(crate) mod fork;
pub(crate) mod git;
pub mod handle;
pub(crate) mod sentinel;
pub(crate) mod startup;

use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};

use llm::{AnthropicClient, LlmClient};
use telemetry::{ProcessId, SessionId};
use tools::{BashDenylist, LaneRouter};

use crate::config::{Config, resolve_api_key};
use crate::error::LooprError;

pub use context::DaemonContext;
pub use handle::DaemonHandle;

/// `GIT_DESCRIBE` of the running binary. Written to
/// `.loopr/daemon.version` on startup so clients can detect version drift.
pub const DAEMON_VERSION: &str = env!("GIT_DESCRIBE");

/// Upper bound on how long we wait for the signal-watcher task to finish
/// after the accept loop has observed shutdown. The watcher's last
/// statement is `notify_waiters` (which wakes the accept loop); by the
/// time the accept loop unwinds back here, the watcher is either already
/// finished or within a few scheduler ticks of finishing. The timeout is
/// defensive — if the watcher registration errored out up front, we
/// skip the wait entirely. This exists so `Arc::try_unwrap(ctx)` below
/// observes the watcher's `Arc<DaemonContext>` clone as already-dropped.
pub const WATCHER_JOIN_TIMEOUT_SECS: u64 = 2;

/// Soft timeout for draining in-flight Implementer tasks at shutdown.
/// Tasks still running after this are `abort_all`'d so the daemon can
/// reach `Arc::try_unwrap(ctx)`. Chosen to be long enough that typical
/// implementer iterations (LLM call ~ a few seconds, tool call ~ sub-
/// second) can complete, short enough that an unresponsive daemon can
/// still be terminated in bounded time.
pub const IMPLEMENTER_DRAIN_TIMEOUT_SECS: u64 = 30;

/// Soft timeout for draining in-flight Reviewer tasks at shutdown.
/// Reviewer is a single LLM turn plus a bounded parse-retry sub-loop;
/// usually finishes within the Anthropic streaming window (~10-30s).
pub const REVIEWER_DRAIN_TIMEOUT_SECS: u64 = 30;

/// Soft timeout for draining in-flight Integrator tasks at shutdown.
/// Integrator is non-LLM (git only); typical path is sub-second. The
/// retry loop's worst case is ~12.6s of backoff, but shutdown cuts the
/// backoff sleep via shutdown_notify, so this budget is a ceiling, not
/// an expected wait.
pub const INTEGRATOR_DRAIN_TIMEOUT_SECS: u64 = 15;

/// Drain `ctx.implementer_tasks` with `IMPLEMENTER_DRAIN_TIMEOUT_SECS`
/// budget. On timeout, `abort_all()` remaining tasks — their
/// `Arc<DaemonContext>` clones release as the abort handles fire.
async fn drain_implementer_tasks<L: LlmClient + Send + Sync + 'static>(ctx: &Arc<DaemonContext<L>>) {
    let mut tasks = ctx.implementer_tasks.lock().await;
    if tasks.is_empty() {
        return;
    }
    let n = tasks.len();
    tracing::info!(count = n, "draining implementer tasks");
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(IMPLEMENTER_DRAIN_TIMEOUT_SECS), drain)
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_secs = IMPLEMENTER_DRAIN_TIMEOUT_SECS,
            remaining = tasks.len(),
            "implementer task drain timed out; aborting remainder"
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

/// Drain `ctx.reviewer_tasks` with `REVIEWER_DRAIN_TIMEOUT_SECS` budget.
/// Runs AFTER `drain_implementer_tasks` so any in-flight Implementer has
/// a chance to enqueue its Reviewer before the Reviewer pool drains.
async fn drain_reviewer_tasks<L: LlmClient + Send + Sync + 'static>(ctx: &Arc<DaemonContext<L>>) {
    let mut tasks = ctx.reviewer_tasks.lock().await;
    if tasks.is_empty() {
        return;
    }
    let n = tasks.len();
    tracing::info!(count = n, "draining reviewer tasks");
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(REVIEWER_DRAIN_TIMEOUT_SECS), drain)
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_secs = REVIEWER_DRAIN_TIMEOUT_SECS,
            remaining = tasks.len(),
            "reviewer task drain timed out; aborting remainder"
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

/// Drain `ctx.integrator_tasks` with `INTEGRATOR_DRAIN_TIMEOUT_SECS`
/// budget. Runs AFTER `drain_reviewer_tasks` so a Reviewer that just
/// reached Accept can enqueue its Integrator before the pool drains.
async fn drain_integrator_tasks<L: LlmClient + Send + Sync + 'static>(ctx: &Arc<DaemonContext<L>>) {
    let mut tasks = ctx.integrator_tasks.lock().await;
    if tasks.is_empty() {
        return;
    }
    let n = tasks.len();
    tracing::info!(count = n, "draining integrator tasks");
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(INTEGRATOR_DRAIN_TIMEOUT_SECS), drain)
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_secs = INTEGRATOR_DRAIN_TIMEOUT_SECS,
            remaining = tasks.len(),
            "integrator task drain timed out; aborting remainder"
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

/// Unconditional parent-side entry: fork a daemon, wait (briefly) for its
/// socket to appear, return. Invoked by `loopr daemon start` (background
/// mode) and by `ensure_daemon_if_needed` when it decides a fork is needed.
///
/// Contract: the parent returns cleanly; the grandchild does NOT return
/// from this function - it runs `daemon_main` and `process::exit`s.
pub fn ensure_daemon(target: &Path) -> Result<(), LooprError> {
    // Clean stale sentinel state first so the grandchild's atomic pid
    // claim can succeed. If a live daemon is already running,
    // `ensure_daemon_if_needed` handles that earlier; here we assume the
    // caller decided a fork is warranted.
    preflight_clean(target);

    match fork::double_fork()? {
        fork::ForkOutcome::Parent => wait_for_socket(target),
        fork::ForkOutcome::Daemon => run_grandchild(target.to_path_buf()),
    }
}

/// Parent-side entry for client commands that need a live daemon.
///
/// If a daemon is already running AND its version matches the binary's,
/// returns `Ok(())` immediately (parent continues normally, never calls
/// `fork`). Otherwise cleans stale sentinel state and forks a fresh
/// daemon. Called BEFORE telemetry init so that the grandchild inherits
/// a clean subscriber state.
pub fn ensure_daemon_if_needed(target: &Path) -> Result<(), LooprError> {
    let pid_file = sentinel::pid_path(target);
    let version_file = sentinel::version_path(target);

    if let Some(pid) = sentinel::read_pid(&pid_file)? {
        if sentinel::is_daemon_alive(pid) && sentinel::version_matches(&version_file, DAEMON_VERSION)? {
            return Ok(());
        }
        // Stale or version-mismatched daemon: try to stop it cleanly.
        let _ = sentinel::kill_stale(target);
    }

    ensure_daemon(target)
}

/// Client-side blocking poll. `connect_or_wait` is the async version used
/// by the transport layer; this is the synchronous equivalent that runs
/// in the parent between fork and telemetry init. We only need to know
/// the socket appeared, not to actually connect.
fn wait_for_socket(target: &Path) -> Result<(), LooprError> {
    let socket = sentinel::socket_path(target);
    let deadline = std::time::Instant::now() + Duration::from_secs(crate::transport::START_TIMEOUT_SECS);
    while std::time::Instant::now() < deadline {
        if socket.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(crate::transport::POLL_INTERVAL_MS));
    }
    Err(LooprError::DaemonStartup(format!(
        "socket never appeared at {}",
        socket.display()
    )))
}

/// Best-effort cleanup of stale sentinel files before `ensure_daemon`
/// tries to claim them. Failures are swallowed; the real authority is
/// the atomic `write_pid` that follows.
fn preflight_clean(target: &Path) {
    let pid_file = sentinel::pid_path(target);
    if let Ok(Some(pid)) = sentinel::read_pid(&pid_file) {
        if !sentinel::is_daemon_alive(pid) {
            sentinel::clean(target);
        }
    } else {
        // No pid file, but a stale socket or version file could still linger.
        sentinel::clean(target);
    }
}

/// Grandchild entry. Create a tokio runtime (MUST happen post-fork, never
/// pre-fork) and block on `daemon_main`. The grandchild never returns to
/// the caller; it `process::exit`s to keep its stack from unwinding back
/// through `lib::run`'s telemetry-init block.
fn run_grandchild(target: PathBuf) -> ! {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(_) => process::exit(1),
    };
    let exit_code = match rt.block_on(daemon_main(target)) {
        Ok(()) => 0,
        Err(LooprError::LockLost) => {
            // Another grandchild won the pid-file race. Silent exit(0);
            // DO NOT call sentinel::clean - the winner's files must stay
            // intact.
            0
        }
        Err(_) => 1,
    };
    process::exit(exit_code);
}

/// Grandchild's async body. Split into two phases per the Phase 3 design:
///
/// 1. **Lock-acquire** (no cleanup on failure): claim the pid file with
///    `O_CREAT | O_EXCL`, then write version + session-id. If pid claim loses
///    the race, return `LockLost` immediately; the caller exits silently.
///
/// 2. **Active-daemon** (cleanup on exit): allocate a `SessionId`, init the
///    daemon's own telemetry subscriber, remove any stale socket file,
///    bind the Unix listener, spawn the signal watcher, run the accept
///    loop. On normal shutdown flush the telemetry guard and
///    `sentinel::clean`.
pub async fn daemon_main(target: PathBuf) -> Result<(), LooprError> {
    let pid = std::process::id();

    // ---- Phase A: lock-acquire (no cleanup on failure) ----
    let pid_file = sentinel::pid_path(&target);
    sentinel::write_pid(&pid_file, pid)?;
    let version_file = sentinel::version_path(&target);
    sentinel::write_version(&version_file, DAEMON_VERSION)?;

    // Resolve (or allocate) this daemon's session, compute the per-target
    // slug, and allocate this process's own id. All three are needed before
    // telemetry::init in Phase B.
    let session_id = crate::session::resolve_session_id(&target, None)?;
    let target_slug =
        telemetry::target_slug(&target).map_err(|e| LooprError::DaemonStartup(format!("target_slug: {e}")))?;
    let process_runs_dir = telemetry::session_target_dir(&session_id, &target_slug)
        .map_err(|e| LooprError::DaemonStartup(format!("session_target_dir: {e}")))?
        .join("runs");
    std::fs::create_dir_all(&process_runs_dir)
        .map_err(|e| LooprError::DaemonStartup(format!("mkdir {}: {e}", process_runs_dir.display())))?;
    let process_id = ProcessId::allocate(&process_runs_dir)
        .map_err(|e| LooprError::DaemonStartup(format!("process id alloc: {e}")))?;
    let process_id_file = sentinel::process_id_path(&target);
    sentinel::write_process_id(&process_id_file, &process_id.to_string())?;

    // ---- Phase B: active-daemon (cleanup on exit) ----
    let outcome = run_active_daemon(target.clone(), session_id, target_slug, process_id, pid).await;

    // Normal shutdown cleanup: only runs if we're the lock winner (we
    // wouldn't have reached here otherwise).
    sentinel::clean(&target);

    outcome
}

/// Phase B body. Isolated so the lock-acquire phase above is obviously
/// cleanup-free.
///
/// ## Shutdown sequence
///
/// 1. `accept_loop` observes `shutting_down` / `shutdown_notify` and
///    drains its handler `JoinSet` with a bounded timeout before
///    returning. Every handler's `Arc<DaemonContext>` clone is released
///    by the time this await resolves.
/// 2. The signal-watcher task is joined with a short timeout so its
///    `Arc<DaemonContext>` clone drops. The watcher's final statement is
///    the `notify_waiters` that woke the accept loop, so by the time the
///    await resolves the task is at most a few scheduler ticks from
///    done; the timeout is defensive for the rare case where the
///    watcher errored out before ever receiving a signal.
/// 3. With every cloned `Arc<DaemonContext>` released, `Arc::try_unwrap`
///    recovers the owned `DaemonContext`, and we call `Store::close`
///    (which consumes `self` and moves the synchronous writer-thread
///    join into a `spawn_blocking` so the tokio reactor is not pinned).
///    If `try_unwrap` unexpectedly finds an outstanding clone we skip
///    close and let the final `Arc::drop` invoke `Store::Drop` — the
///    crash-interrupt fallback path that the store is explicitly
///    designed to tolerate as best-effort only.
async fn run_active_daemon(
    target: PathBuf,
    session_id: SessionId,
    target_slug: String,
    process_id: ProcessId,
    pid: u32,
) -> Result<(), LooprError> {
    // Init the daemon's own telemetry subscriber. Safe because `lib::run`'s
    // pre-telemetry hoist guarantees the parent never called
    // `set_global_default`; the COW'd "already set" flag is false in the
    // grandchild's memory.
    let directive = std::env::var(telemetry::LOG_ENV_VAR).unwrap_or_else(|_| "info".to_string());
    let _guard = telemetry::init(&target, &session_id, &target_slug, &process_id, &directive)
        .map_err(|e| LooprError::DaemonStartup(format!("telemetry init: {e}")))?;

    // Load top-level Config (composes each stage's config) and build the
    // process-wide AnthropicClient. Config missing from `.loopr/config.yml`
    // falls back to defaults; API key missing from env falls back to a
    // placeholder that keeps the daemon booting but makes real LLM calls
    // fail with 401. See `crate::config` for the degradation contract.
    let config = Config::load(&target)?;
    let api_key = resolve_api_key(&config.llm);
    let anthropic = AnthropicClient::new(config.llm.clone(), api_key)
        .map_err(|e| LooprError::DaemonStartup(format!("anthropic client: {e}")))?;

    let ctx = build_context(target, session_id, target_slug, process_id, pid, anthropic, config).await?;
    serve(ctx).await
}

/// Construct a `DaemonContext<L>` ready for `serve` / `serve_core`. This
/// is the test-reachable entry point for the pipeline: tests call it with
/// a stub `L` and a `Config::default()`, skipping `Config::load` and the
/// production `AnthropicClient` construction.
///
/// Side effects (all contained to this function, mirror `run_active_daemon`'s
/// historical behavior): opens the per-target `Store`, installs
/// `.git/info/exclude` patterns, builds `LaneRouter` + `BashDenylist` +
/// `path_deny_patterns` from config, runs the startup reconcile sweep.
/// Fails if the Store cannot open or the lane router fails sandbox detection.
pub async fn build_context<L>(
    target: PathBuf,
    session_id: SessionId,
    target_slug: String,
    process_id: ProcessId,
    pid: u32,
    llm: L,
    config: Config,
) -> Result<Arc<DaemonContext<L>>, LooprError>
where
    L: LlmClient + Send + Sync + 'static,
{
    // Open the per-target store AFTER telemetry init so open errors land
    // in the daemon's run log, and BEFORE `DaemonContext::new` because the
    // context owns the store for the duration of the active phase.
    let store = store::Store::open(&target)
        .await
        .map_err(|e| LooprError::DaemonStartup(format!("store open: {e}")))?;

    // Install `.git/info/exclude` patterns once per boot. Idempotent; safe to
    // call on every start. Failure here is non-fatal — a missing `.git/info/`
    // (non-git target) means we couldn't be managing worktrees anyway, and
    // the guard layer (`crate::guard`) has already rejected non-git targets.
    if let Err(e) = worktree::ensure_loopr_excludes(&target) {
        tracing::warn!(error = %e, "daemon startup: ensure_loopr_excludes failed (non-fatal)");
    }

    // Build the tool infrastructure BEFORE DaemonContext::new. LaneRouter::new
    // is fallible when `sandbox: required` and bwrap is not functional on
    // this host; surface that as a daemon startup failure with an actionable
    // message so the client sees the recovery path (install bubblewrap, or
    // downgrade `.loopr/config.yml tools.sandbox` to `preferred`).
    let sandbox = config.tools.sandbox;
    let router = Arc::new(LaneRouter::new(sandbox).map_err(|e| {
        LooprError::DaemonStartup(format!(
            "tool lane router: {e}. Install bubblewrap (`apt install bubblewrap`) or set \
             `.loopr/config.yml`: `tools: {{ sandbox: preferred }}`."
        ))
    })?);
    let mut bash_denylist = BashDenylist::with_base();
    bash_denylist.extend_from(&config.tools);
    let bash_denylist = Arc::new(bash_denylist);

    // Path-deny patterns: defaults + target extensions. These apply to every
    // file-touching tool (Read/Write/Edit/Grep/Glob) regardless of sandbox
    // posture.
    let mut path_deny_patterns: Vec<String> = [".env", ".key", ".pem", "credentials", "secret"]
        .into_iter()
        .map(str::to_string)
        .collect();
    path_deny_patterns.extend(config.tools.path_deny_patterns.iter().cloned());

    let context_builder = Arc::new(::context::InlineContextBuilder::new());
    let implementer_config = ::agents::ImplementerConfig::default();
    let reviewer_config = ::agents::ReviewerConfig::default();
    let integrator_config = ::integrator::IntegratorConfig::default();
    let worktree_cleanup_policy = config.worktree.cleanup_policy;

    let ctx = Arc::new(DaemonContext::new(
        target.clone(),
        session_id,
        target_slug,
        process_id,
        pid,
        store,
        Arc::new(llm),
        router,
        bash_denylist,
        path_deny_patterns,
        sandbox,
        context_builder,
        implementer_config,
        reviewer_config,
        integrator_config,
        worktree_cleanup_policy,
    ));

    tracing::info!(
        target_dir = %target.display(),
        session_id = %ctx.session_id,
        pid = ctx.pid,
        "daemon.started"
    );

    // Hygiene sweep: clean up worktrees left behind by a previous daemon
    // crash, log orphans. Runs BEFORE the accept loop binds so no
    // coordinator session can race with this pass.
    let report = startup::reconcile(&ctx).await?;
    tracing::info!(
        cleaned = report.cleaned,
        orphans = report.orphans_logged,
        carried_forward = report.carried_forward,
        foreign = report.foreign_skipped,
        "daemon.startup.reconcile.complete"
    );

    Ok(ctx)
}

/// Pipeline body. Binds the IPC socket, runs the accept loop, drains the
/// three task pools, and returns the `Arc<DaemonContext<L>>` for the caller
/// to close.
///
/// NO signal handlers installed — shutdown is driven exclusively by
/// `ctx.shutting_down` + `ctx.shutdown_notify`. Test harness calls this
/// directly without a signal watcher.
///
/// Returns the Arc so `Arc::try_unwrap` + `store.close().await` happen at
/// the outer layer, AFTER the caller has joined any other Arc holders
/// (e.g. production's signal watcher). Closing the store inside this
/// function would deterministically fail `try_unwrap` when production's
/// watcher still holds a clone.
pub async fn serve_core<L>(ctx: Arc<DaemonContext<L>>) -> Result<Arc<DaemonContext<L>>, LooprError>
where
    L: LlmClient + Send + Sync + 'static,
{
    // Unconditionally remove any stale socket file before bind. The PID
    // lock we already hold is the authority; a lingering socket file on
    // disk is just a side-effect of a previous ungraceful exit.
    let socket = sentinel::socket_path(&ctx.target);
    if socket.exists() {
        std::fs::remove_file(&socket)
            .map_err(|e| LooprError::DaemonStartup(format!("remove stale socket {}: {e}", socket.display())))?;
    }

    let listener = crate::transport::server::bind_listener(&socket)?;

    // Phase 4: real accept loop. Upgraded at Stage 5 to hold a JoinSet of
    // per-connection handlers and drain it before returning (see
    // `crate::transport::server::accept_loop`).
    let accept_result = crate::transport::server::accept_loop(listener, ctx.clone()).await;

    // Stage 7 wiring: drain in-flight Implementer tasks. Each task holds
    // an `Arc<DaemonContext>` clone (the `self: Arc<Self>` parameter of
    // `spawn_implementer_for_work`); they MUST complete before
    // `Arc::try_unwrap` in the caller, or the Store falls back to its sync
    // Drop which can panic on the tokio runtime.
    drain_implementer_tasks(&ctx).await;

    // Stage 8 wiring: drain Reviewer tasks AFTER Implementer tasks so a
    // successful Implementer on the wire can enqueue its Reviewer into
    // the pool before the Reviewer drain begins.
    drain_reviewer_tasks(&ctx).await;

    // Stage 8 wiring: drain Integrator tasks AFTER Reviewer tasks so an
    // in-flight Reviewer with an Accept verdict can enqueue its
    // Integrator before the pool drains.
    drain_integrator_tasks(&ctx).await;

    accept_result?;
    Ok(ctx)
}

/// Production wrapper around `serve_core`: installs the SIGTERM/SIGINT
/// watcher, calls `serve_core`, joins the watcher so its `Arc<DaemonContext>`
/// clone drops, then `try_unwrap`s + `store.close().await`s. Not used by
/// tests (would collide with the test runner's signal handlers).
pub async fn serve<L>(ctx: Arc<DaemonContext<L>>) -> Result<(), LooprError>
where
    L: LlmClient + Send + Sync + 'static,
{
    // Signal-watcher task. Awaits SIGTERM/SIGINT as async values; sets
    // shutting_down + notify_waiters on first signal. No POSIX
    // signal-handler-safety concerns.
    let watcher_handle = spawn_signal_watcher(ctx.clone());

    let ctx = serve_core(ctx).await?;

    // Wait for the signal watcher to finish so its `Arc<DaemonContext>`
    // clone drops. Bounded by WATCHER_JOIN_TIMEOUT_SECS in case the
    // watcher never observed a signal (e.g. registration error).
    let _ = tokio::time::timeout(Duration::from_secs(WATCHER_JOIN_TIMEOUT_SECS), watcher_handle).await;

    // Every other `Arc<DaemonContext>` clone (accept loop's parameter,
    // watcher task, every handler task) should be dropped by now. Try to
    // unwrap the Arc to recover the owned Store and close it async.
    match Arc::try_unwrap(ctx) {
        Ok(owned) => match owned.store.close().await {
            Ok(()) => tracing::info!("daemon.store.closed"),
            Err(e) => tracing::warn!(error = %e, "daemon.store.close failed"),
        },
        Err(still_shared) => {
            tracing::warn!(
                strong_count = Arc::strong_count(&still_shared),
                "DaemonContext Arc still shared at shutdown; falling back to Store::Drop"
            );
        }
    }

    Ok(())
}

/// Install the SIGTERM / SIGINT watcher. On first signal: set
/// `shutting_down = true` and notify every awaiter of `shutdown_notify`.
/// Returns the spawned `JoinHandle` so `run_active_daemon` can await it
/// during shutdown and release the watcher's `Arc<DaemonContext>` clone
/// before `Arc::try_unwrap`.
fn spawn_signal_watcher<L: LlmClient + Send + Sync + 'static>(
    ctx: Arc<DaemonContext<L>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("signal watcher: failed to register SIGTERM: {e}");
                return;
            }
        };
        let mut intr = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("signal watcher: failed to register SIGINT: {e}");
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => tracing::info!("signal watcher: SIGTERM received"),
            _ = intr.recv() => tracing::info!("signal watcher: SIGINT received"),
        }
        ctx.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
        ctx.shutdown_notify.notify_waiters();
    })
}
