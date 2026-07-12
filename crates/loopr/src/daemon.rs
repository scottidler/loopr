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

pub mod context;
pub(crate) mod fork;
pub(crate) mod git;
pub mod handle;
pub(crate) mod sentinel;
pub(crate) mod startup;
pub(crate) mod summary_fanout;

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

/// Soft timeout for draining in-flight Director tasks at shutdown.
/// Director makes Opus LLM calls; budget is sized to allow a single
/// in-flight Opus completion to return cleanly. Sleep is interruptible
/// via `shutdown_notify` so an idle/poll wait never burns this budget.
pub const DIRECTOR_DRAIN_TIMEOUT_SECS: u64 = 30;

/// Soft timeout for draining in-flight `WorkSpawner` tasks at shutdown.
/// Spawner tasks do a Store read + small mutation + (in
/// `accept_bundle`) one Integrator spawn — non-LLM, sub-second
/// typical. The 10s budget is a ceiling, not an expected wait.
pub const WORK_SPAWNER_DRAIN_TIMEOUT_SECS: u64 = 10;

/// Soft timeout for draining in-flight `plan.create` decompose tasks at
/// shutdown. The task makes a decompose LLM call then persists Works and
/// spawns the initial Implementers + Director, so it is sized like the
/// LLM-bearing pools. Drained FIRST (root of the spawn DAG) so its
/// children land in their pools before those pools drain. A hung
/// decompose cannot block shutdown past this ceiling.
pub const PLAN_CREATE_DRAIN_TIMEOUT_SECS: u64 = 30;

/// Worst-case wall-clock for a graceful shutdown, derived from the drains
/// that `serve_core` runs SEQUENTIALLY plus the watcher + reaper joins in
/// `serve`. This is the floor the SIGTERM->SIGKILL escalation window in
/// `sentinel::kill_stale` must clear (+ margin) so `daemon stop` and
/// version-drift auto-kill never SIGKILL a daemon that is still inside its
/// legitimate drain budget mid-LLM-call. Each pool drain has its own
/// internal abort-on-timeout, so a wedged daemon still exits within this
/// bound; the escalation window is the backstop for a daemon that ignores
/// SIGTERM entirely.
pub const GRACEFUL_SHUTDOWN_BUDGET_SECS: u64 = crate::transport::server::HANDLER_DRAIN_TIMEOUT_SECS
    + PLAN_CREATE_DRAIN_TIMEOUT_SECS
    + IMPLEMENTER_DRAIN_TIMEOUT_SECS
    + REVIEWER_DRAIN_TIMEOUT_SECS
    + DIRECTOR_DRAIN_TIMEOUT_SECS
    + WORK_SPAWNER_DRAIN_TIMEOUT_SECS
    + INTEGRATOR_DRAIN_TIMEOUT_SECS
    + 2 * WATCHER_JOIN_TIMEOUT_SECS;

/// Install the process-wide panic hook so a panicking pipeline task does
/// not vanish silently. The daemon's stdio is `/dev/null` post-fork, so
/// the default libstd hook (which writes to stderr) produces zero
/// evidence; a panicking Implementer would strand its Work with no trace.
/// This hook routes the panic payload + location through `tracing::error!`
/// so it lands in the daemon's `events.log`. Installed AFTER
/// `telemetry::init` so the subscriber is live. The previous hook is
/// chained so the default behavior (and any test harness hook) still runs.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        tracing::error!(location = %location, payload = %payload, "daemon task panicked");
        previous(info);
    }));
}

/// Drain a JoinSet pool with a bounded timeout, logging any panicked
/// (non-cancelled) task via `warn!` so a swallowed panic leaves a trace.
/// On timeout, `abort_all()` the remainder; aborted tasks resolve to
/// `Cancelled` join errors, which are expected and not logged. Shared by
/// the six pool-specific drain helpers so the panic-visibility contract is
/// identical across pools.
async fn drain_pool(tasks: &mut tokio::task::JoinSet<()>, timeout_secs: u64, pool: &'static str) {
    let drain = async {
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res
                && !e.is_cancelled()
            {
                tracing::warn!(pool, error = %e, "task panicked during shutdown drain");
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(timeout_secs), drain)
        .await
        .is_err()
    {
        tracing::warn!(
            pool,
            timeout_secs,
            remaining = tasks.len(),
            "task drain timed out; aborting remainder"
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

/// Reap completed tasks from a locked pool without blocking, logging any
/// panicked (non-cancelled) task. Lets finished tasks (and their
/// `Arc<DaemonContext>` clones) release during a long run instead of
/// accumulating in the JoinSet until shutdown drain, and surfaces a
/// mid-run panic promptly rather than waiting for the drain.
fn reap_finished(tasks: &mut tokio::task::JoinSet<()>, pool: &'static str) {
    while let Some(res) = tasks.try_join_next() {
        if let Err(e) = res
            && !e.is_cancelled()
        {
            tracing::warn!(pool, error = %e, "task panicked (reaped mid-run)");
        }
    }
}

/// Cadence at which the background reaper sweeps every pool for finished
/// tasks. The pools never receive new spawns after their shutdown drain
/// returns, so this only matters during the active phase; 30s keeps the
/// JoinSets from growing unbounded under a long multi-Work run without
/// adding meaningful contention on the per-pool mutexes.
pub const POOL_REAP_INTERVAL_SECS: u64 = 30;

/// Reap every pipeline pool once. Acquires each pool's mutex briefly,
/// drains its finished tasks, and releases. Mutex contention with the
/// spawn paths and the shutdown drains is negligible (microsecond reaps).
async fn reap_all_pools<L: LlmClient + Send + Sync + 'static>(ctx: &Arc<DaemonContext<L>>) {
    let pools: [(&tokio::sync::Mutex<tokio::task::JoinSet<()>>, &'static str); 6] = [
        (&ctx.plan_create_tasks, "plan-create"),
        (&ctx.implementer_tasks, "implementer"),
        (&ctx.reviewer_tasks, "reviewer"),
        (&ctx.director_tasks, "director"),
        (&ctx.work_spawner_tasks, "work-spawner"),
        (&ctx.integrator_tasks, "integrator"),
    ];
    for (pool, name) in pools {
        let mut tasks = pool.lock().await;
        reap_finished(&mut tasks, name);
    }
    // Phase 18: prune the keyed abort-handle map alongside the JoinSet reap,
    // so finished Implementers' `AbortHandle`s do not accumulate.
    ctx.prune_finished_abort_handles();
}

/// Spawn the background pool reaper. Wakes every `POOL_REAP_INTERVAL_SECS`
/// to reap finished tasks across all pools; exits promptly on shutdown so
/// its `Arc<DaemonContext>` clone drops before `serve`'s `Arc::try_unwrap`.
/// Returns the handle so `serve` can join it alongside the signal watcher.
/// Not spawned by `serve_core` (the test path), which drains pools
/// explicitly.
fn spawn_pool_reaper<L: LlmClient + Send + Sync + 'static>(ctx: Arc<DaemonContext<L>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(POOL_REAP_INTERVAL_SECS));
        // Consume the immediate first tick so the first real sweep is one
        // full interval out.
        interval.tick().await;
        loop {
            if ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            tokio::select! {
                biased;
                _ = ctx.shutdown_notify.notified() => return,
                _ = interval.tick() => reap_all_pools(&ctx).await,
            }
        }
    })
}

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
    drain_pool(&mut tasks, IMPLEMENTER_DRAIN_TIMEOUT_SECS, "implementer").await;
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
    drain_pool(&mut tasks, REVIEWER_DRAIN_TIMEOUT_SECS, "reviewer").await;
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
    drain_pool(&mut tasks, INTEGRATOR_DRAIN_TIMEOUT_SECS, "integrator").await;
}

/// Drain `ctx.director_tasks` with `DIRECTOR_DRAIN_TIMEOUT_SECS` budget.
/// Runs AFTER `drain_integrator_tasks` so a Director that just dispatched
/// a final `accept_bundle` (which spawns into the Integrator pool) lets
/// the Integrator land before the Director itself winds down. The
/// Director's sleep is `tokio::select!` against `shutdown_notify`, so
/// this drain typically completes in milliseconds — the timeout is a
/// ceiling for the case where a Director is mid-Opus call when shutdown
/// fires.
async fn drain_director_tasks<L: LlmClient + Send + Sync + 'static>(ctx: &Arc<DaemonContext<L>>) {
    let mut tasks = ctx.director_tasks.lock().await;
    if tasks.is_empty() {
        return;
    }
    let n = tasks.len();
    tracing::info!(count = n, "draining director tasks");
    drain_pool(&mut tasks, DIRECTOR_DRAIN_TIMEOUT_SECS, "director").await;
}

/// Drain `ctx.work_spawner_tasks` with `WORK_SPAWNER_DRAIN_TIMEOUT_SECS`
/// budget. Runs AFTER `drain_director_tasks` so a Director that
/// dispatched a final action (`accept_bundle` / `override_work` /
/// `assign_work`) has its spawner task land in this pool before the
/// drain. Runs BEFORE `drain_integrator_tasks` so an
/// `accept_bundle` that spawns an Integrator can enqueue it before
/// that pool drains.
async fn drain_work_spawner_tasks<L: LlmClient + Send + Sync + 'static>(ctx: &Arc<DaemonContext<L>>) {
    let mut tasks = ctx.work_spawner_tasks.lock().await;
    if tasks.is_empty() {
        return;
    }
    let n = tasks.len();
    tracing::info!(count = n, "draining work-spawner tasks");
    drain_pool(&mut tasks, WORK_SPAWNER_DRAIN_TIMEOUT_SECS, "work-spawner").await;
}

/// Drain `ctx.plan_create_tasks` with `PLAN_CREATE_DRAIN_TIMEOUT_SECS`
/// budget. Runs FIRST in the shutdown sequence, ahead of every other
/// pool: a `plan.create` task is the root of the spawn DAG (it spawns
/// Implementers and a Director), so draining it first lets those
/// children enqueue into their own pools before those pools drain,
/// preserving the "no pool receives a new spawn after its drain returns"
/// invariant. On timeout (a hung decompose), abort the remainder so
/// shutdown is never blocked.
async fn drain_plan_create_tasks<L: LlmClient + Send + Sync + 'static>(ctx: &Arc<DaemonContext<L>>) {
    let mut tasks = ctx.plan_create_tasks.lock().await;
    if tasks.is_empty() {
        return;
    }
    let n = tasks.len();
    tracing::info!(count = n, "draining plan-create tasks");
    drain_pool(&mut tasks, PLAN_CREATE_DRAIN_TIMEOUT_SECS, "plan-create").await;
}

/// Unconditional parent-side entry: fork a daemon, wait (briefly) for its
/// socket to appear, return. Invoked by `loopr daemon start` (background
/// mode) and by `ensure_daemon_if_needed` when it decides a fork is needed.
///
/// Contract: the parent returns cleanly; the grandchild does NOT return
/// from this function - it runs `daemon_main` and `process::exit`s.
pub fn ensure_daemon(target: &Path, accept_corruption: bool) -> Result<(), LooprError> {
    // Clean stale sentinel state first so the grandchild's atomic pid
    // claim can succeed. If a live daemon is already running,
    // `ensure_daemon_if_needed` handles that earlier; here we assume the
    // caller decided a fork is warranted.
    preflight_clean(target);

    match fork::double_fork()? {
        fork::ForkOutcome::Parent => wait_for_socket(target),
        fork::ForkOutcome::Daemon => run_grandchild(target.to_path_buf(), accept_corruption),
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

    // Auto-fork triggered by a client subcommand — the operator never had a
    // chance to opt into corruption-tolerant boot, so default to gated.
    ensure_daemon(target, false)
}

/// Read-verb guard (Phase 16 of `docs/design/2026-07-11-verified-swarm.md`):
/// reports whether a live daemon is running WITHOUT forking one. Every
/// read verb (`daemon status`, `plans`/`works`/`bundles`/`ticks`, `show`,
/// `budget reset`) calls this instead of `ensure_daemon_if_needed` so a
/// read can never silently become a fork; a read verb that finds `false`
/// prints "no daemon running" and returns `Ok(())` without ever calling
/// `connect_or_wait`.
pub fn is_running(target: &Path) -> Result<bool, LooprError> {
    let pid_file = sentinel::pid_path(target);
    match sentinel::read_pid(&pid_file)? {
        Some(pid) if sentinel::is_daemon_alive(pid) => Ok(true),
        _ => Ok(false),
    }
}

/// Client-side blocking poll. `connect_or_wait` is the async version used
/// by the transport layer; this is the synchronous equivalent that runs
/// in the parent between fork and telemetry init. We only need to know
/// the socket appeared, not to actually connect.
fn wait_for_socket(target: &Path) -> Result<(), LooprError> {
    let socket = sentinel::socket_path(target);
    // Honor the operator-tunable client-connect budget (defaults to the
    // daemon startup budget), not a hard 3s: this parent poll sits on the
    // same fork path a client takes, so a slow crash-recovery reconcile
    // must not trip it. Best-effort config load; a broken config falls
    // back to the default budget (the daemon's own startup surfaces the
    // config error strictly).
    let wait = match crate::config::Config::load(target) {
        Ok(cfg) => Duration::from_secs(cfg.transport.client_connect_secs),
        Err(e) => {
            tracing::warn!(error = %e, "config load failed; using default client connect budget");
            Duration::from_secs(crate::config::DEFAULT_STARTUP_BUDGET_SECS)
        }
    };
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline {
        if socket.exists() {
            return Ok(());
        }
        // Fail fast (and with the real reason) if the grandchild recorded a
        // startup failure. `preflight_clean` removed any stale file before
        // the fork, so a present sentinel is from THIS boot.
        if let Some(reason) = sentinel::read_startup_error(target) {
            return Err(LooprError::DaemonStartup(format!("daemon failed to start: {reason}")));
        }
        std::thread::sleep(Duration::from_millis(crate::transport::POLL_INTERVAL_MS));
    }
    // Timed out with no startup-error sentinel — surface whatever the
    // grandchild may have recorded right at the deadline, else the generic
    // socket-never-appeared message.
    let suffix = sentinel::read_startup_error(target)
        .map(|r| format!("; daemon reported: {r}"))
        .unwrap_or_default();
    Err(LooprError::DaemonStartup(format!(
        "socket never appeared at {}{suffix}",
        socket.display()
    )))
}

/// Best-effort cleanup of stale sentinel files before a daemon tries to
/// claim them. Failures are swallowed; the real authority is the atomic
/// `write_pid` that follows. Called by `ensure_daemon` (background fork)
/// and by the foreground `daemon start --foreground` branch in `lib::run`
/// (which IS the daemon and does not go through `ensure_daemon`).
pub(crate) fn preflight_clean(target: &Path) {
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
fn run_grandchild(target: PathBuf, accept_corruption: bool) -> ! {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(_) => process::exit(1),
    };
    let exit_code = match rt.block_on(daemon_main(target.clone(), accept_corruption)) {
        Ok(()) => 0,
        Err(LooprError::LockLost) => {
            // Another grandchild won the pid-file race. Silent exit(0);
            // DO NOT call sentinel::clean or write a startup-error - the
            // winner's files must stay intact.
            0
        }
        Err(LooprError::CorruptionGate { count }) => {
            // Daemon refused to start because reconcile surfaced corrupt
            // JSONL rows. Stable non-zero exit code lets scripts detect
            // the gate-trip without parsing stderr. The startup-error
            // sentinel surfaces the reason to the parent (whose stdio
            // sees nothing post-fork). Written AFTER daemon_main's own
            // sentinel::clean so it survives.
            let reason = format!("refusing to start: {count} corrupt record(s)");
            sentinel::write_startup_error(&target, &reason);
            eprintln!("daemon: {reason}");
            CORRUPTION_GATE_EXIT_CODE
        }
        Err(e) => {
            let reason = format!("startup failed: {e}");
            sentinel::write_startup_error(&target, &reason);
            eprintln!("daemon: {reason}");
            1
        }
    };
    process::exit(exit_code);
}

/// Stable exit code for `LooprError::CorruptionGate`. Tests + scripts can
/// match on this without parsing stderr. Distinct from the generic `1`.
const CORRUPTION_GATE_EXIT_CODE: i32 = 78;

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
pub async fn daemon_main(target: PathBuf, accept_corruption: bool) -> Result<(), LooprError> {
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
    let outcome = run_active_daemon(
        target.clone(),
        session_id,
        target_slug,
        process_id,
        pid,
        accept_corruption,
    )
    .await;

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
#[tracing::instrument(
    name = "daemon.run_active",
    level = "info",
    skip_all,
    fields(
        target = %target.display(),
        session_id = %session_id,
        process_id = %process_id,
        target_slug = %target_slug,
        pid,
    ),
    err,
)]
async fn run_active_daemon(
    target: PathBuf,
    session_id: SessionId,
    target_slug: String,
    process_id: ProcessId,
    pid: u32,
    accept_corruption: bool,
) -> Result<(), LooprError> {
    // Init the daemon's own telemetry subscriber. Safe because `lib::run`'s
    // pre-telemetry hoist guarantees the parent never called
    // `set_global_default`; the COW'd "already set" flag is false in the
    // grandchild's memory.
    let directive = std::env::var(telemetry::LOG_ENV_VAR).unwrap_or_else(|_| "info".to_string());
    let _guard = telemetry::init(&target, &session_id, &target_slug, &process_id, &directive)
        .map_err(|e| LooprError::DaemonStartup(format!("telemetry init: {e}")))?;

    // Route task panics through tracing now that the subscriber is live.
    // Daemon stdio is `/dev/null` post-fork, so without this a panicking
    // pipeline task would leave zero evidence in the run log.
    install_panic_hook();

    // One-shot legacy-state detector. Fires once per daemon boot; no-op
    // on fresh targets or targets already cleaned with `rkvr rmrf`.
    startup::check_legacy_runs_dir(&target);

    // Load top-level Config (composes each stage's config) and build the
    // process-wide AnthropicClient. Config missing from `.loopr/config.yml`
    // falls back to defaults; API key missing from env falls back to a
    // placeholder that keeps the daemon booting but makes real LLM calls
    // fail with 401. See `crate::config` for the degradation contract.
    let config = Config::load(&target)?;
    let api_key = resolve_api_key(&config.llm);
    let anthropic = AnthropicClient::new(config.llm.clone(), api_key)
        .map_err(|e| LooprError::DaemonStartup(format!("anthropic client: {e}")))?;

    // Phase 7: process-wide ProcessSnapshot accumulates counters
    // through the daemon's lifetime; wrap the AnthropicClient in
    // MeteredLlmClient so every LLM call's Usage feeds the snapshot.
    // The snapshot is also handed to DaemonContext for non-LLM
    // counters (plan/work/bundle/tick lifecycle).
    let snapshot = Arc::new(std::sync::Mutex::new(telemetry::digest::process::ProcessSnapshot::new(
        config.llm.model.clone(),
    )));
    // Phase 6: append one line per LLM call to `<target>/.loopr/costs.jsonl`
    // (vision cost audit). The CostSink carries the run-id (this PID's
    // process id); each call's Plan/Work/role come from the CallContext
    // task-local the spawn task bodies install.
    let cost_sink = Arc::new(llm::CostSink::new(&target.join(".loopr"), process_id.as_str()));
    let metered = llm::MeteredLlmClient::with_costs(anthropic, Arc::clone(&snapshot), cost_sink);

    // Phase B startup watchdog: bound `build_context` (Store::open +
    // worktree::ensure_loopr_excludes + startup::reconcile) so a hung
    // disk operation surfaces as a real error in the run log instead
    // of orphaning the grandchild before it ever binds the socket.
    // `serve` is intentionally OUTSIDE the wrap — the accept loop runs
    // for the whole daemon lifetime.
    let startup_budget = Duration::from_secs(config.transport.daemon_startup_secs);
    let ctx = bound_startup(
        startup_budget,
        build_context(
            target,
            session_id,
            target_slug,
            process_id,
            pid,
            metered,
            config,
            accept_corruption,
            Arc::clone(&snapshot),
        ),
    )
    .await?;
    serve(ctx).await
}

/// Wrap a startup future with the configured budget. On timeout, surface
/// `LooprError::DaemonStartup` so the run log records the budget breach
/// and `daemon_main` cleans up sentinel files in its Phase B exit path.
pub async fn bound_startup<F, T>(budget: Duration, fut: F) -> Result<T, LooprError>
where
    F: std::future::Future<Output = Result<T, LooprError>>,
{
    match tokio::time::timeout(budget, fut).await {
        Ok(res) => res,
        Err(_elapsed) => Err(LooprError::DaemonStartup(format!(
            "build_context exceeded {}s startup budget",
            budget.as_secs()
        ))),
    }
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
#[tracing::instrument(
    name = "daemon.build_context",
    level = "info",
    skip_all,
    fields(
        target = %target.display(),
        session_id = %session_id,
        process_id = %process_id,
        target_slug = %target_slug,
        pid,
    ),
    err,
)]
pub async fn build_context<L>(
    target: PathBuf,
    session_id: SessionId,
    target_slug: String,
    process_id: ProcessId,
    pid: u32,
    llm: L,
    config: Config,
    accept_corruption: bool,
    snapshot: Arc<std::sync::Mutex<telemetry::digest::process::ProcessSnapshot>>,
) -> Result<Arc<DaemonContext<L>>, LooprError>
where
    L: LlmClient + Send + Sync + 'static,
{
    // Phase 12 (validation-by-default): fail closed BEFORE any other
    // side effect (store open, sandbox probe, LLM spend) when this
    // target's integrator config cannot produce executed proof. This is
    // the single choke point shared by production (`run_active_daemon`)
    // and the test-reachable entry point documented above, so both get
    // the same fail-closed gate. A config problem is not a per-Bundle
    // terminal failure; it is a startup refusal with a named knob.
    config.integrator.validate()?;

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
    let router = Arc::new(LaneRouter::with_config(sandbox, &config.tools).map_err(|e| {
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

    let prompt_loader = Arc::new(::context::PromptLoader::for_target(&target).map_err(|e| {
        LooprError::DaemonStartup(format!("prompt loader construction failed for target {target:?}: {e}"))
    })?);
    let context_builder = Arc::new(::context::InlineContextBuilder::with_loader(prompt_loader));
    // Overlay the canonical per-Work budget (top-level `budgets:`) onto
    // the implementer config's programmatic carrier, so the budget config
    // lives in one place (`budgets.per-work-cost-usd`) but reaches the
    // implementer loop's per-Work cost brake.
    let mut implementer_config = config.agents.implementer.clone();
    implementer_config.per_work_cost_cap_usd = config.budgets.per_work_cost_usd;
    // Phase 3 (llm/agents defect sweep) added `ImplementerConfig::validate()`
    // to reject a negative/NaN `per_work_cost_cap_usd`, but had no seam of
    // its own into loopr's config-load path. Phase 4 closes that here: this
    // overlay is the only place `per_work_cost_cap_usd` is set from
    // operator-controlled config (`budgets.per-work-cost-usd`), so validating
    // immediately after the overlay — and BEFORE `DaemonContext::new` runs —
    // fails daemon startup loudly and closed instead of letting an invalid
    // cap reach the per-Work runtime brake (`(cap * 1e6) as u64` on a
    // negative/NaN cap saturates to 0 and escalates every Work instantly).
    implementer_config
        .validate()
        .map_err(|e| LooprError::DaemonStartup(format!("agents.implementer config: {e}")))?;
    let reviewer_config = config.agents.reviewer.clone();
    let director_config = config.agents.director.clone();
    let decomposer_config = config.decomposer.clone();
    let server_timeouts = crate::transport::ServerTimeouts::from(&config.transport);
    let integrator_config = config.integrator.into_integrator_config();
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
        director_config,
        decomposer_config,
        worktree_cleanup_policy,
        snapshot,
        server_timeouts,
        config.budgets.per_run_cost_usd,
        config.budgets.max_concurrent_implementers,
    ));

    tracing::info!(
        target_dir = %target.display(),
        session_id = %ctx.session_id,
        pid = ctx.pid,
        "daemon.started"
    );

    // Hygiene sweep + corruption gate + pipeline-resume sweeps. The
    // corruption gate now lives INSIDE `reconcile`, between its scan phase
    // and its spawn phase, so a corrupt store never has reviewers /
    // integrators / Directors spawned against it. The snapshot
    // corruption-count mirror is also done inside `reconcile`. A gated
    // boot surfaces here as `LooprError::CorruptionGate` via `?`.
    let report = startup::reconcile(&ctx, accept_corruption).await?;
    tracing::info!(
        cleaned = report.cleaned,
        orphans = report.orphans_logged,
        carried_forward = report.carried_forward,
        foreign = report.foreign_skipped,
        corruption_count = report.corruption_count,
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
#[tracing::instrument(
    name = "daemon.serve_core",
    level = "info",
    skip_all,
    fields(target = %ctx.target.display(), session_id = %ctx.session_id, process_id = %ctx.process_id),
    err,
)]
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

    // Drain in reverse-spawn-chain order so no pool can receive a NEW
    // spawn after its drain has returned. Spawn DAG today:
    //
    //   Implementer ─spawns─▶ Reviewer ─spawns─▶ Integrator
    //                                              ▲
    //   Director    ─spawns─▶ work_spawner ────────┘
    //                              │
    //                              └─ spawns Integrator (accept_bundle)
    //
    // Reverse-toposort drain: implementer → reviewer → director →
    // work_spawner → integrator. Each task in a downstream pool holds
    // an `Arc<DaemonContext>` clone (via `Arc::clone(&self.0)` or the
    // `self: Arc<Self>` receiver); they MUST complete before
    // `Arc::try_unwrap` in `serve` reclaims the Store, or Store falls
    // back to its sync Drop which can panic on the tokio runtime.
    //
    // See docs/design/2026-05-09-director-phase-1-followups.md "Drain
    // Ordering Rationale" for why this differs from v0.7.11's
    // (defensively-correct but structurally-muddled)
    // implementer→reviewer→integrator→director order.
    drain_plan_create_tasks(&ctx).await;
    drain_implementer_tasks(&ctx).await;
    drain_reviewer_tasks(&ctx).await;
    drain_director_tasks(&ctx).await;
    drain_work_spawner_tasks(&ctx).await;
    drain_integrator_tasks(&ctx).await;

    // Phase 7: write the per-process digest after the task pools
    // drain and before the caller's `Arc::try_unwrap`. Best-effort:
    // failures emit `warn!` and proceed; the rest of the shutdown
    // sequence still runs.
    write_process_digest_best_effort(&ctx);

    accept_result?;
    Ok(ctx)
}

/// Compute the per-process digest path under XDG and write the
/// rendered digest. Best-effort: any error emits `warn!` and the
/// shutdown continues.
fn write_process_digest_best_effort<L>(ctx: &Arc<DaemonContext<L>>)
where
    L: LlmClient + Send + Sync + 'static,
{
    let run_dir = match telemetry::session_run_dir(&ctx.session_id, &ctx.target_slug, &ctx.process_id) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "session_run_dir failed; skipping per-process digest");
            return;
        }
    };
    let snap = match ctx.snapshot.lock() {
        Ok(g) => g.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    match telemetry::digest::process::write_process_digest(&run_dir, &snap) {
        Ok(path) => tracing::info!(path = %path.display(), "daemon.digest.process.written"),
        Err(e) => tracing::warn!(error = %e, "daemon.digest.process.write_failed"),
    }
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

    // Background pool reaper: keeps the JoinSets from accumulating finished
    // tasks during a long run and surfaces mid-run panics. Joined below
    // (before `try_unwrap`) so its `Arc<DaemonContext>` clone drops.
    let reaper_handle = spawn_pool_reaper(ctx.clone());

    let ctx = serve_core(ctx).await?;

    // Wait for the signal watcher and the pool reaper to finish so their
    // `Arc<DaemonContext>` clones drop. Both observe `shutting_down` /
    // `shutdown_notify` set by the watcher, so they exit within a few
    // scheduler ticks; the timeout is defensive.
    let _ = tokio::time::timeout(Duration::from_secs(WATCHER_JOIN_TIMEOUT_SECS), watcher_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(WATCHER_JOIN_TIMEOUT_SECS), reaper_handle).await;

    // Every other `Arc<DaemonContext>` clone (accept loop's parameter,
    // watcher task, every handler task) should be dropped by now. Try
    // to unwrap the Arc to recover the owned Store and close it async.
    //
    // Phase 6 introduced `Arc<Store>` ownership inside DaemonContext
    // (the SummaryFanout decorator holds two Arc<Store> clones
    // alongside the field). The shutdown sequence is now:
    //
    //   1. `Arc::try_unwrap(ctx)`           — owns the DaemonContext
    //   2. clone `owned.store` (bumps Arc<Store> strong_count)
    //   3. `drop(owned)`                    — releases owned.store +
    //                                         the 2 SummaryFanout
    //                                         clones inside owned
    //   4. `Arc::try_unwrap(store_clone)`   — owns the underlying Store
    //   5. `store.close().await`            — async close
    //
    // INVARIANT: every Arc<Store> clone outside of `DaemonContext`
    // (i.e. anywhere other than `ctx.store` and the two clones inside
    // `ctx.summary_fanout`) MUST drop before this shutdown's first
    // `try_unwrap`. A future contributor adding a long-lived
    // Arc<Store> clone elsewhere will trip the second `try_unwrap`'s
    // fallback path.
    match Arc::try_unwrap(ctx) {
        Ok(owned) => {
            let store_clone = Arc::clone(&owned.store);
            drop(owned);
            match Arc::try_unwrap(store_clone) {
                Ok(store) => match store.close().await {
                    Ok(()) => tracing::info!("daemon.store.closed"),
                    Err(e) => tracing::warn!(error = %e, "daemon.store.close failed"),
                },
                Err(still_shared) => {
                    tracing::warn!(
                        strong_count = Arc::strong_count(&still_shared),
                        "Arc<Store> still shared after DaemonContext drop; falling back to Store::Drop"
                    );
                }
            }
        }
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

#[cfg(test)]
mod tests;
