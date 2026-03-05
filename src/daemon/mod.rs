pub mod context;
pub mod handlers;
pub mod supervisor;
pub mod work_queue;

use std::sync::Arc;
use std::time::Duration;

use eyre::eyre;
use log::{debug, error, info, warn};
use tokio::net::UnixListener;
use tokio::sync::RwLock;

use crate::agents::AgentStatus;
use crate::config::Config;
use crate::ipc::protocol::DaemonEvent;
use crate::ipc::server::{self, IpcServer};

use self::context::{DaemonContext, Stores};

/// Check if the daemon is running; if not, double-fork to spawn it in the
/// background and wait for the socket to appear. Uses proper Unix
/// daemonization (fork → setsid → fork) so the daemon is fully detached
/// from the spawning terminal session.
///
/// IMPORTANT: This function must be called BEFORE any Tokio runtime is
/// created. The double-fork requires a single-threaded process to be safe.
/// The grandchild (daemon) creates its own Tokio runtime post-fork.
pub fn ensure_daemon(config: &Config, log_level: Option<&str>) -> eyre::Result<()> {
    let pid_path = &config.daemon.pid_path;
    let socket_path = &config.daemon.socket_path;

    // Check PID file — is a daemon already alive?
    if let Ok(contents) = std::fs::read_to_string(pid_path)
        && let Ok(pid) = contents.trim().parse::<u32>()
        && std::path::Path::new(&format!("/proc/{pid}")).exists()
    {
        // Daemon is running — check socket exists too
        if socket_path.exists() {
            return Ok(());
        }
        // PID alive but socket missing — daemon may still be starting up
        eprintln!("Daemon process alive (pid={pid}) but socket not ready, waiting...");
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if socket_path.exists() {
                return Ok(());
            }
        }
        return Err(eyre::eyre!(
            "daemon process alive (pid={pid}) but socket never appeared at {}",
            socket_path.display()
        ));
    }

    // No live daemon — double-fork daemonize
    eprintln!("Starting daemon...");

    // SAFETY: No Tokio runtime exists at this point (main is sync).
    // Fork is safe in a single-threaded process before any runtime setup.
    unsafe {
        // First fork
        let pid = libc::fork();
        if pid < 0 {
            return Err(eyre!("first fork failed: {}", std::io::Error::last_os_error()));
        }
        if pid > 0 {
            // Parent: reap first child (exits immediately), then wait for socket
            let mut status: libc::c_int = 0;
            libc::waitpid(pid, &mut status, 0);

            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if socket_path.exists() {
                    return Ok(());
                }
            }
            return Err(eyre!(
                "daemon started but socket never appeared at {}",
                socket_path.display()
            ));
        }

        // First child: become session leader (detach from terminal)
        if libc::setsid() < 0 {
            libc::_exit(1);
        }

        // Second fork: grandchild cannot acquire a controlling terminal
        let pid2 = libc::fork();
        if pid2 < 0 {
            libc::_exit(1);
        }
        if pid2 > 0 {
            // First child: exit immediately
            libc::_exit(0);
        }

        // Grandchild: redirect stdio to /dev/null
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, libc::STDIN_FILENO);
            libc::dup2(devnull, libc::STDOUT_FILENO);
            libc::dup2(devnull, libc::STDERR_FILENO);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
    }

    // === Grandchild (daemon) only from here ===
    let session_id = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let log_path = crate::setup_logging(config, log_level, Some(&session_id)).expect("daemon: failed to setup logging");
    let session_dir = log_path.parent().map(std::path::PathBuf::from).unwrap_or_default();

    info!(
        "Daemon started via double-fork (session: {}, pid: {})",
        session_id,
        std::process::id()
    );

    let rt = tokio::runtime::Runtime::new().expect("daemon: failed to create Tokio runtime");
    let result = rt.block_on(async {
        let (ctx, _) = DaemonContext::shared(config.clone(), session_id, session_dir)?;
        daemon_main(ctx).await
    });

    // Grandchild exits here — NEVER returns to caller
    std::process::exit(match result {
        Ok(()) => 0,
        Err(e) => {
            error!("daemon exited with error: {}", e);
            1
        }
    });
}

/// Ensure only one daemon runs at a time. If a stale PID file exists from a
/// dead process, clean it up. If a live daemon is found, abort with an error.
fn ensure_one_daemon(ctx: &DaemonContext) -> eyre::Result<()> {
    let pid_path = &ctx.config.daemon.pid_path;
    if let Ok(contents) = std::fs::read_to_string(pid_path)
        && let Ok(pid) = contents.trim().parse::<u32>()
    {
        let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
        if proc_path.exists() {
            return Err(eyre!(
                "daemon already running (pid={pid}, pidfile={})",
                pid_path.display()
            ));
        }
        warn!("Stale PID file found (pid={pid}), cleaning up");
        let _ = std::fs::remove_file(pid_path);
    }
    // Also clean up stale socket
    let socket_path = &ctx.config.daemon.socket_path;
    if socket_path.exists() {
        warn!("Stale socket found, cleaning up: {}", socket_path.display());
        let _ = std::fs::remove_file(socket_path);
    }
    Ok(())
}

/// Write the daemon PID file.
fn write_pid_file(ctx: &DaemonContext) -> std::io::Result<()> {
    let pid = std::process::id();
    if let Some(parent) = ctx.config.daemon.pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&ctx.config.daemon.pid_path, pid.to_string())?;
    info!("Wrote PID file: {} (pid={})", ctx.config.daemon.pid_path.display(), pid);
    Ok(())
}

/// Write daemon version file alongside the PID file.
fn write_version_file(ctx: &DaemonContext) -> std::io::Result<()> {
    let runtime_dir = ctx
        .config
        .daemon
        .pid_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let version_path = runtime_dir.join("daemon.version");
    std::fs::write(&version_path, env!("CARGO_PKG_VERSION"))?;
    info!("Wrote version file: {}", version_path.display());
    Ok(())
}

/// Remove the daemon PID file.
fn remove_pid_file(ctx: &DaemonContext) {
    let _ = std::fs::remove_file(&ctx.config.daemon.pid_path);
    info!("Removed PID file: {}", ctx.config.daemon.pid_path.display());
}

/// Main daemon entry point.
/// Binds the Unix socket, accepts client connections, and runs the select! loop
/// until SIGINT (ctrl_c) is received.
pub async fn daemon_main(ctx: Arc<RwLock<DaemonContext>>) -> eyre::Result<()> {
    debug!("daemon_main()");
    let socket_path = {
        let c = ctx.read().await;
        ensure_one_daemon(&c)?;
        write_pid_file(&c)?;
        write_version_file(&c)?;
        // Crash recovery: reset any orphaned InProgress/Integrating records
        c.recover_orphaned_records();
        c.config.daemon.socket_path.clone()
    };

    let ipc_server = IpcServer::new(&socket_path);
    let listener = ipc_server.bind().await?;
    info!("Daemon listening on {}", ipc_server.socket_path().display());
    let event_tx = ctx.read().await.event_tx.clone();

    // Gap #31: Auto-start coordinator at daemon startup
    {
        let c = ctx.read().await;
        if c.config.agents.auto_start_coordinator {
            let start_req = crate::ipc::protocol::DaemonRequest::new(
                0,
                "agent.start",
                serde_json::json!({ "agent_type": "coordinator" }),
            );
            let _ = crate::daemon::handlers::dispatch(
                &c.stores,
                &c.event_tx,
                &c.worktree_manager,
                &c.config.integrator,
                start_req,
            );
            info!("Auto-started Coordinator agent");
        }
        // Auto-start Integrator when enabled
        if c.config.integrator.enabled {
            let start_req = crate::ipc::protocol::DaemonRequest::new(
                0,
                "agent.start",
                serde_json::json!({ "agent_type": "integrator" }),
            );
            let _ = crate::daemon::handlers::dispatch(
                &c.stores,
                &c.event_tx,
                &c.worktree_manager,
                &c.config.integrator,
                start_req,
            );
            info!("Auto-started Integrator");
        }
    }

    // Spawn pull-based worker pool when enabled
    let mut worker_handles = Vec::new();
    {
        let c = ctx.read().await;
        if c.config.agents.pull_based_workers && c.config.agents.enabled {
            let pool_size = c.config.agents.worker_pool_size;
            info!("Spawning {} pull-based workers", pool_size);
            for i in 0..pool_size {
                let s = c.stores.clone();
                let e = event_tx.clone();
                let w = c.worktree_manager.clone();
                let ic = c.config.agents.implementer.clone();
                let wc = crate::agents::worker::WorkerConfig {
                    worker_id: i,
                    poll_interval_secs: 5,
                    idle_interval_secs: 15,
                };
                worker_handles.push(tokio::spawn(crate::agents::worker::run_worker(s, e, w, ic, wc)));
            }
        }
    }

    // Spawn coordinator supervisor — watches for coordinator failures and restarts with backoff
    let supervisor_handle = {
        let c = ctx.read().await;
        let stores = c.stores.clone();
        let evt = event_tx.clone();
        let wm = c.worktree_manager.clone();
        let ic = c.config.integrator.clone();
        tokio::spawn(supervisor::run_supervisor(
            stores,
            evt,
            wm,
            ic,
            supervisor::SupervisorConfig::default(),
        ))
    };

    let result = accept_loop(listener, ctx.clone(), event_tx.clone()).await;

    // Abort the supervisor task on shutdown
    supervisor_handle.abort();

    // Signal workers to shut down
    {
        let c = ctx.read().await;
        c.stores.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    for handle in worker_handles {
        handle.abort();
    }

    // Graceful shutdown: cancel agent sessions, wait for tasks, abort stragglers
    {
        let c = ctx.read().await;
        graceful_shutdown(&c.stores, &event_tx).await;
    }

    // Generate session summary before cleanup
    {
        let c = ctx.read().await;
        let start_time = format!(
            "{}-{}-{}T{}:{}:{}",
            &c.session_id[..4],
            &c.session_id[4..6],
            &c.session_id[6..8],
            &c.session_id[9..11],
            &c.session_id[11..13],
            &c.session_id[13..15],
        );
        crate::session_summary::generate_summary(&c.session_dir, &c.session_id, &start_time);
        eprintln!(
            "loopr session {} ended. Run `loopr diagnose dump` for diagnostics.",
            c.session_id
        );
    }

    // Cleanup on shutdown
    ipc_server.cleanup();
    {
        let c = ctx.read().await;
        remove_pid_file(&c);
    }
    info!("Daemon shut down cleanly");

    result
}

/// Graceful shutdown: cancel all agent sessions, wait for tasks to exit,
/// then abort any remaining tasks after a grace period.
async fn graceful_shutdown(stores: &Arc<Stores>, event_tx: &tokio::sync::broadcast::Sender<DaemonEvent>) {
    let grace_period = Duration::from_secs(10);
    info!("Starting graceful shutdown, grace period: {:?}", grace_period);

    // 1. Broadcast shutting_down event
    let _ = event_tx.send(DaemonEvent::new("system.shutting_down", serde_json::json!({})));

    // 2. Cancel all non-terminal agent sessions
    {
        let Ok(mut sessions) = stores.write_agent_sessions() else {
            error!("agent_sessions lock poisoned during shutdown");
            return;
        };
        for session in sessions.values_mut() {
            if !session.status.is_terminal() {
                let _ = session.transition_to(AgentStatus::Cancelled);
            }
        }
    }

    // 3. Drain handles and wait with timeout
    let handles: Vec<_> = {
        let Ok(mut handle_map) = stores.lock_agent_handles() else {
            error!("agent_handles lock poisoned during shutdown");
            return;
        };
        handle_map.drain().collect()
    };

    if handles.is_empty() {
        info!("No agent tasks to wait for");
        return;
    }

    info!("Waiting for {} agent task(s) to exit", handles.len());

    let mut join_set = tokio::task::JoinSet::new();
    for (id, handle) in handles {
        join_set.spawn(async move {
            let result = handle.await;
            (id, result)
        });
    }

    match tokio::time::timeout(grace_period, async {
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((id, Ok(()))) => info!("Agent task {} exited gracefully", id),
                Ok((id, Err(e))) => warn!("Agent task {} join error: {}", id, e),
                Err(e) => warn!("JoinSet error: {}", e),
            }
        }
    })
    .await
    {
        Ok(()) => info!("All agent tasks exited gracefully"),
        Err(_) => {
            warn!("Grace period expired, aborting remaining tasks");
            join_set.abort_all();
        }
    }
}

/// Accept loop: handles incoming connections, SIGINT/SIGTERM, and IPC shutdown.
async fn accept_loop(
    listener: UnixListener,
    ctx: Arc<RwLock<DaemonContext>>,
    event_tx: tokio::sync::broadcast::Sender<DaemonEvent>,
) -> eyre::Result<()> {
    let mut shutdown_rx = event_tx.subscribe();

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        let event_rx = event_tx.subscribe();
                        // Extract stores, worktree_manager, and event_tx for the handler closure
                        let (stores, worktree_mgr, integrator_config) = {
                            let c = ctx.read().await;
                            (c.stores.clone(), c.worktree_manager.clone(), c.config.integrator.clone())
                        };
                        let handler_event_tx = event_tx.clone();
                        tokio::spawn(async move {
                            server::handle_client(
                                stream,
                                move |req| {
                                    handlers::dispatch(&stores, &handler_event_tx, &worktree_mgr, &integrator_config, req)
                                },
                                event_rx,
                            ).await;
                        });
                    }
                    Err(e) => {
                        log::error!("Failed to accept connection: {}", e);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down");
                break;
            }
            event = shutdown_rx.recv() => {
                if let Ok(ev) = event
                    && ev.event == "system.shutdown"
                {
                    info!("Received shutdown command via IPC");
                    break;
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ipc::client::IpcClient;
    use crate::test_util::TestDir;
    use serde_json::json;

    fn test_config() -> (TestDir, Config) {
        let dir = TestDir::new("loopr-daemon-test");
        let config = Config {
            daemon: crate::config::DaemonConfig {
                socket_path: dir.join("test.sock"),
                pid_path: dir.join("test.pid"),
            },
            project: crate::config::ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..crate::config::ProjectConfig::default()
            },
            ..Config::default()
        };
        (dir, config)
    }

    #[tokio::test]
    async fn test_write_and_remove_pid_file() {
        let (_dir, config) = test_config();
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let ctx = context::DaemonContext::new(
            config.clone(),
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        write_pid_file(&ctx).unwrap();
        assert!(config.daemon.pid_path.exists());
        let contents = std::fs::read_to_string(&config.daemon.pid_path).unwrap();
        assert_eq!(contents, std::process::id().to_string());

        remove_pid_file(&ctx);
        assert!(!config.daemon.pid_path.exists());
    }

    #[tokio::test]
    async fn test_daemon_handshake() {
        let (_dir, config) = test_config();
        let socket_path = config.daemon.socket_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(
            config,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // Start daemon in background
        let daemon_handle = tokio::spawn(daemon_main(ctx));

        // Wait briefly for daemon to bind
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Connect client and send handshake
        let mut client = IpcClient::connect(&socket_path).await.unwrap();
        let resp = client.handshake("0.1.0").await.unwrap();
        assert_eq!(resp.result.as_ref().unwrap()["protocol"], "ndjson/1");

        // Unknown method returns error
        let (resp2, _events) = client.request("unknown.method", json!(null)).await.unwrap();
        assert!(resp2.is_error());
        assert!(resp2.error.as_ref().unwrap().message.contains("unknown.method"));

        drop(client);
        daemon_handle.abort();
        // Cleanup socket if it remains
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn test_daemon_pid_file_lifecycle() {
        let (_dir, config) = test_config();
        let pid_path = config.daemon.pid_path.clone();
        let socket_path = config.daemon.socket_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(
            config,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        let daemon_handle = tokio::spawn(daemon_main(ctx));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // PID file should exist while daemon is running
        assert!(pid_path.exists());
        let pid_str = std::fs::read_to_string(&pid_path).unwrap();
        assert!(!pid_str.is_empty());

        daemon_handle.abort();
        let _ = daemon_handle.await;
        // Cleanup
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&pid_path);
    }

    #[tokio::test]
    async fn test_daemon_multiple_clients() {
        let (_dir, config) = test_config();
        let socket_path = config.daemon.socket_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(
            config,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        let daemon_handle = tokio::spawn(daemon_main(ctx));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Connect two clients simultaneously
        let mut client1 = IpcClient::connect(&socket_path).await.unwrap();
        let mut client2 = IpcClient::connect(&socket_path).await.unwrap();

        let resp1 = client1.handshake("0.1.0").await.unwrap();
        let resp2 = client2.handshake("0.1.0").await.unwrap();

        assert!(!resp1.is_error());
        assert!(!resp2.is_error());

        drop(client1);
        drop(client2);
        daemon_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn test_daemon_status() {
        let (_dir, config) = test_config();
        let socket_path = config.daemon.socket_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(
            config,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        let daemon_handle = tokio::spawn(daemon_main(ctx));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut client = IpcClient::connect(&socket_path).await.unwrap();
        let _ = client.handshake("0.1.0").await.unwrap();

        let (resp, _events) = client.request("system.status", json!(null)).await.unwrap();
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["version"].is_string());
        assert!(result["pid"].is_number());
        assert_eq!(result["counts"]["plans"], 0);

        drop(client);
        daemon_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn test_ensure_one_daemon_stale_pid_cleanup() {
        let (_dir, config) = test_config();
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let ctx = context::DaemonContext::new(
            config.clone(),
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // Write a PID file with a PID that does not exist (stale)
        std::fs::create_dir_all(config.daemon.pid_path.parent().unwrap()).unwrap();
        std::fs::write(&config.daemon.pid_path, "999999999").unwrap();
        assert!(config.daemon.pid_path.exists());

        // ensure_one_daemon should clean up the stale PID file and succeed
        let result = ensure_one_daemon(&ctx);
        assert!(result.is_ok());
        // Stale PID file should be removed
        assert!(!config.daemon.pid_path.exists());
    }

    #[tokio::test]
    async fn test_ensure_one_daemon_live_daemon_errors() {
        let (_dir, config) = test_config();
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let ctx = context::DaemonContext::new(
            config.clone(),
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // Write the current process PID — /proc/<our_pid> exists, so it looks alive
        let our_pid = std::process::id();
        std::fs::create_dir_all(config.daemon.pid_path.parent().unwrap()).unwrap();
        std::fs::write(&config.daemon.pid_path, our_pid.to_string()).unwrap();

        let result = ensure_one_daemon(&ctx);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("daemon already running"));
        assert!(err_msg.contains(&our_pid.to_string()));
    }

    #[tokio::test]
    async fn test_ensure_one_daemon_stale_socket_cleanup() {
        let (_dir, config) = test_config();
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let ctx = context::DaemonContext::new(
            config.clone(),
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // No PID file, but a stale socket exists
        std::fs::create_dir_all(config.daemon.socket_path.parent().unwrap()).unwrap();
        std::fs::write(&config.daemon.socket_path, "stale").unwrap();
        assert!(config.daemon.socket_path.exists());

        let result = ensure_one_daemon(&ctx);
        assert!(result.is_ok());
        // Stale socket should be removed
        assert!(!config.daemon.socket_path.exists());
    }

    #[tokio::test]
    async fn test_write_version_file() {
        let (_dir, config) = test_config();
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let ctx = context::DaemonContext::new(
            config.clone(),
            tx,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        write_version_file(&ctx).unwrap();

        let runtime_dir = config.daemon.pid_path.parent().unwrap();
        let version_path = runtime_dir.join("daemon.version");
        assert!(version_path.exists());
        let contents = std::fs::read_to_string(&version_path).unwrap();
        assert_eq!(contents, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn test_graceful_shutdown_no_handles() {
        let stores = Arc::new(context::Stores::new());
        let (event_tx, _rx) = tokio::sync::broadcast::channel::<DaemonEvent>(16);

        // No agent handles — should return immediately without hanging
        graceful_shutdown(&stores, &event_tx).await;

        // Verify that the shutting_down event was sent (subscriber would have received it)
        // The function returned without error — that's the main assertion
    }

    #[tokio::test]
    async fn test_graceful_shutdown_with_handles() {
        let stores = Arc::new(context::Stores::new());
        let (event_tx, _rx) = tokio::sync::broadcast::channel::<DaemonEvent>(16);

        // Insert a Running agent session that should be cancelled
        {
            let mut sessions = stores.agent_sessions.write().unwrap();
            let mut session =
                crate::agents::AgentSession::new(crate::agents::AgentType::Implementer, "test-model".to_string());
            session.status = AgentStatus::Running;
            sessions.insert(session.id.clone(), session);
        }

        // Insert a mock agent handle (a quick task that completes)
        {
            let handle = tokio::spawn(async {
                // Simulate agent work that finishes quickly
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            });
            let mut handles = stores.agent_handles.lock().unwrap();
            handles.insert("test-session".to_string(), handle);
        }

        graceful_shutdown(&stores, &event_tx).await;

        // Agent sessions should have been cancelled
        let sessions = stores.agent_sessions.read().unwrap();
        for session in sessions.values() {
            assert!(
                session.status.is_terminal(),
                "expected terminal status, got {:?}",
                session.status
            );
        }

        // Agent handles should have been drained
        let handles = stores.agent_handles.lock().unwrap();
        assert!(handles.is_empty());
    }

    #[tokio::test]
    async fn test_daemon_ipc_shutdown() {
        let (_dir, config) = test_config();
        let socket_path = config.daemon.socket_path.clone();
        let pid_path = config.daemon.pid_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(
            config,
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        let daemon_handle = tokio::spawn(daemon_main(ctx));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut client = IpcClient::connect(&socket_path).await.unwrap();
        let _ = client.handshake("0.1.0").await.unwrap();

        // Send shutdown command
        let (resp, _events) = client.request("system.shutdown", json!(null)).await.unwrap();
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "shutting_down");

        // Daemon should exit on its own
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), daemon_handle).await;
        assert!(result.is_ok(), "daemon should have shut down");

        // Cleanup
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&pid_path);
    }

    #[tokio::test]
    async fn test_ensure_daemon_already_running() {
        // When a daemon is already running (PID alive + socket exists),
        // ensure_daemon should return Ok immediately without forking.
        let (_dir, config) = test_config();
        let socket_path = config.daemon.socket_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(
            config.clone(),
            "test".into(),
            std::path::PathBuf::from("/tmp/loopr-test-session"),
        )
        .unwrap();

        // Start a real daemon
        let daemon_handle = tokio::spawn(daemon_main(ctx));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // ensure_daemon should detect the running daemon and return Ok
        let result = ensure_daemon(&config, None);
        assert!(result.is_ok(), "ensure_daemon should detect running daemon");

        daemon_handle.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[test]
    fn test_ensure_daemon_stale_pid_no_socket() {
        // When PID file points to a dead process and no socket exists,
        // ensure_daemon would attempt to double-fork. We can't easily test
        // the fork path in unit tests, so instead test the "no PID file" path
        // which also triggers the fork. Since we can't fork in tests, we just
        // verify the PID-check logic by testing with a live PID + missing socket.
        let (_dir, config) = test_config();

        // Write our own PID (alive) but no socket → should timeout waiting for socket
        std::fs::create_dir_all(config.daemon.pid_path.parent().unwrap()).unwrap();
        std::fs::write(&config.daemon.pid_path, std::process::id().to_string()).unwrap();

        let result = ensure_daemon(&config, None);
        assert!(result.is_err(), "should error when PID alive but no socket");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("socket never appeared"));
    }
}
