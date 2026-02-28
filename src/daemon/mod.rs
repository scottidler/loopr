pub mod context;
pub mod handlers;

use std::sync::Arc;
use std::time::Duration;

use eyre::{Context, eyre};
use log::{info, warn};
use tokio::net::UnixListener;
use tokio::sync::RwLock;

use crate::agents::AgentStatus;
use crate::ipc::protocol::DaemonEvent;
use crate::ipc::server::{self, IpcServer};

use self::context::{DaemonContext, Stores};

/// Check if the daemon is running; if not, spawn it in the background and wait
/// for the socket to appear. This lets `loopr` (TUI) and CLI commands work
/// without requiring a separate `loopr daemon` step.
pub fn ensure_daemon(pid_path: &std::path::Path, socket_path: &std::path::Path) -> eyre::Result<()> {
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
        info!("Daemon process alive (pid={pid}) but socket not ready, waiting...");
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

    // No live daemon — spawn one
    eprintln!("Starting daemon...");
    let exe = std::env::current_exe().context("failed to determine loopr executable path")?;
    std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn daemon process")?;

    // Wait for socket to appear (up to 3 seconds)
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if socket_path.exists() {
            return Ok(());
        }
    }

    Err(eyre::eyre!(
        "daemon was spawned but socket never appeared at {}",
        socket_path.display()
    ))
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

/// Remove the daemon PID file.
fn remove_pid_file(ctx: &DaemonContext) {
    let _ = std::fs::remove_file(&ctx.config.daemon.pid_path);
    info!("Removed PID file: {}", ctx.config.daemon.pid_path.display());
}

/// Main daemon entry point.
/// Binds the Unix socket, accepts client connections, and runs the select! loop
/// until SIGINT (ctrl_c) is received.
pub async fn daemon_main(ctx: Arc<RwLock<DaemonContext>>) -> eyre::Result<()> {
    let socket_path = {
        let c = ctx.read().await;
        ensure_one_daemon(&c)?;
        write_pid_file(&c)?;
        // Crash recovery: reset any orphaned InProgress/Integrating records
        c.recover_orphaned_records();
        c.config.daemon.socket_path.clone()
    };

    let ipc_server = IpcServer::new(&socket_path);
    let listener = ipc_server.bind().await?;
    info!("Daemon listening on {}", ipc_server.socket_path().display());
    let event_tx = ctx.read().await.event_tx.clone();

    let result = accept_loop(listener, ctx.clone(), event_tx.clone()).await;

    // Graceful shutdown: cancel agent sessions, wait for tasks, abort stragglers
    {
        let c = ctx.read().await;
        graceful_shutdown(&c.stores, &event_tx).await;
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
        let mut sessions = stores.agent_sessions.write().unwrap();
        for session in sessions.values_mut() {
            if !session.status.is_terminal() {
                let _ = session.transition_to(AgentStatus::Cancelled);
            }
        }
    }

    // 3. Drain handles and wait with timeout
    let handles: Vec<_> = {
        let mut handle_map = stores.agent_handles.lock().unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ipc::client::IpcClient;
    use serde_json::json;

    fn test_config() -> Config {
        let id = crate::id::generate_id();
        let dir = std::env::temp_dir().join(format!("loopr-daemon-test-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        Config {
            daemon: crate::config::DaemonConfig {
                socket_path: dir.join("test.sock"),
                pid_path: dir.join("test.pid"),
            },
            project: crate::config::ProjectConfig {
                repo_path: dir.clone(),
                ..crate::config::ProjectConfig::default()
            },
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn test_write_and_remove_pid_file() {
        let config = test_config();
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let ctx = context::DaemonContext::new(config.clone(), tx).unwrap();

        write_pid_file(&ctx).unwrap();
        assert!(config.daemon.pid_path.exists());
        let contents = std::fs::read_to_string(&config.daemon.pid_path).unwrap();
        assert_eq!(contents, std::process::id().to_string());

        remove_pid_file(&ctx);
        assert!(!config.daemon.pid_path.exists());
    }

    #[tokio::test]
    async fn test_daemon_handshake() {
        let config = test_config();
        let socket_path = config.daemon.socket_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(config).unwrap();

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
        let config = test_config();
        let pid_path = config.daemon.pid_path.clone();
        let socket_path = config.daemon.socket_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(config).unwrap();

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
        let config = test_config();
        let socket_path = config.daemon.socket_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(config).unwrap();

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
        let config = test_config();
        let socket_path = config.daemon.socket_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(config).unwrap();

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
    async fn test_daemon_ipc_shutdown() {
        let config = test_config();
        let socket_path = config.daemon.socket_path.clone();
        let pid_path = config.daemon.pid_path.clone();
        let (ctx, _tx) = context::DaemonContext::shared(config).unwrap();

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
}
