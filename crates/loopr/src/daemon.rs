//! Daemon process lifecycle: double-fork detachment, pid/version/run-id
//! sentinels, signal handling, run loop. Owned by the `loopr` driver crate;
//! the `transport` module hangs off the daemon's accept loop but does not
//! know about the fork or the pid file.
//!
//! Submodules: `fork` (libc double-fork primitive), `sentinel` (pid /
//! version / run-id / socket filesystem helpers), `context` (`DaemonContext`
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
pub(crate) mod sentinel;

use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};

use telemetry::RunId;

use crate::error::LooprError;

pub use context::DaemonContext;

/// `GIT_DESCRIBE` of the running binary. Written to
/// `.loopr/daemon.version` on startup so clients can detect version drift.
pub const DAEMON_VERSION: &str = env!("GIT_DESCRIBE");

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
///    `O_CREAT | O_EXCL`, then write version + run-id. If pid claim loses
///    the race, return `LockLost` immediately; the caller exits silently.
///
/// 2. **Active-daemon** (cleanup on exit): allocate a `RunId`, init the
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

    // Allocate the run-id now so the sentinel file can point at it, and
    // pass the same id into the telemetry subscriber in Phase B.
    let runs_dir = target.join(".loopr").join("runs");
    std::fs::create_dir_all(&runs_dir)
        .map_err(|e| LooprError::DaemonStartup(format!("mkdir {}: {e}", runs_dir.display())))?;
    let run_id = RunId::allocate(&runs_dir).map_err(|e| LooprError::DaemonStartup(format!("run id alloc: {e}")))?;
    let run_id_file = sentinel::run_id_path(&target);
    sentinel::write_run_id(&run_id_file, &run_id.to_string())?;

    // ---- Phase B: active-daemon (cleanup on exit) ----
    let outcome = run_active_daemon(target.clone(), run_id, pid).await;

    // Normal shutdown cleanup: only runs if we're the lock winner (we
    // wouldn't have reached here otherwise).
    sentinel::clean(&target);

    outcome
}

/// Phase B body. Isolated so the lock-acquire phase above is obviously
/// cleanup-free.
async fn run_active_daemon(target: PathBuf, run_id: RunId, pid: u32) -> Result<(), LooprError> {
    // Init the daemon's own telemetry subscriber. Safe because `lib::run`'s
    // pre-telemetry hoist guarantees the parent never called
    // `set_global_default`; the COW'd "already set" flag is false in the
    // grandchild's memory.
    let directive = std::env::var(telemetry::LOG_ENV_VAR).unwrap_or_else(|_| "info".to_string());
    let _guard = telemetry::init(&target, &run_id, &directive)
        .map_err(|e| LooprError::DaemonStartup(format!("telemetry init: {e}")))?;

    let ctx = Arc::new(DaemonContext::new(target.clone(), run_id, pid));

    tracing::info!(
        target_dir = %target.display(),
        run_id = %ctx.run_id,
        pid = ctx.pid,
        "daemon.started"
    );

    // Unconditionally remove any stale socket file before bind. The PID
    // lock we already hold is the authority; a lingering socket file on
    // disk is just a side-effect of a previous ungraceful exit.
    let socket = sentinel::socket_path(&target);
    if socket.exists() {
        std::fs::remove_file(&socket)
            .map_err(|e| LooprError::DaemonStartup(format!("remove stale socket {}: {e}", socket.display())))?;
    }

    let listener = crate::transport::server::bind_listener(&socket)?;

    // Signal-watcher task. Awaits SIGTERM/SIGINT as async values; sets
    // shutting_down + notify_waiters on first signal. No POSIX
    // signal-handler-safety concerns.
    spawn_signal_watcher(ctx.clone());

    // Phase 4: real accept loop. Spawns one per-connection handler task.
    crate::transport::server::accept_loop(listener, ctx).await?;
    Ok(())
}

/// Install the SIGTERM / SIGINT watcher. On first signal: set
/// `shutting_down = true` and notify every awaiter of `shutdown_notify`.
fn spawn_signal_watcher(ctx: Arc<DaemonContext>) {
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
    });
}
