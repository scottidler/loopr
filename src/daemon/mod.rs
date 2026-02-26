pub mod context;
pub mod handlers;

use std::sync::Arc;

use log::info;
use tokio::net::UnixListener;
use tokio::sync::RwLock;

use crate::ipc::protocol::DaemonEvent;
use crate::ipc::server::{self, IpcServer};

use self::context::DaemonContext;

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
        write_pid_file(&c)?;
        c.config.daemon.socket_path.clone()
    };

    let (ipc_server, _) = IpcServer::new(&socket_path);
    let listener = ipc_server.bind().await?;
    info!("Daemon listening on {}", ipc_server.socket_path().display());
    let event_tx = ipc_server.event_sender();

    let result = accept_loop(listener, ctx.clone(), event_tx).await;

    // Cleanup on shutdown
    ipc_server.cleanup();
    {
        let c = ctx.read().await;
        remove_pid_file(&c);
    }
    info!("Daemon shut down cleanly");

    result
}

/// Accept loop: handles incoming connections and ctrl_c for graceful shutdown.
async fn accept_loop(
    listener: UnixListener,
    ctx: Arc<RwLock<DaemonContext>>,
    event_tx: tokio::sync::broadcast::Sender<DaemonEvent>,
) -> eyre::Result<()> {
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
        let dir = std::env::temp_dir().join("loopr-daemon-test");
        std::fs::create_dir_all(&dir).unwrap();
        let id = crate::id::generate_id();
        Config {
            daemon: crate::config::DaemonConfig {
                socket_path: dir.join(format!("test-{id}.sock")),
                pid_path: dir.join(format!("test-{id}.pid")),
            },
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn test_write_and_remove_pid_file() {
        let config = test_config();
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let ctx = context::DaemonContext::new(config.clone(), tx);

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
        let (ctx, _tx) = context::DaemonContext::shared(config);

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
        let (ctx, _tx) = context::DaemonContext::shared(config);

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
        let (ctx, _tx) = context::DaemonContext::shared(config);

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
}
