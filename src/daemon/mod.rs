//! Daemon Core - scheduler, tick loop, and crash recovery
//!
//! The daemon is the long-running process that:
//! - Schedules and executes loops based on priority
//! - Runs a tick loop to process pending work
//! - Recovers from crashes by restoring interrupted loops

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::info;
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::RwLock;

use crate::error::{LooprError, Result};
use crate::ipc::messages::{DaemonError, DaemonRequest, DaemonResponse, Methods};
use crate::ipc::server::{IpcServer, IpcServerConfig, RequestHandler};

pub mod context;
pub mod handlers;
pub mod recovery;
pub mod scheduler;
pub mod tick;

pub use context::*;
pub use handlers::*;
pub use recovery::*;
pub use scheduler::*;
pub use tick::*;

/// Get the default data directory (~/.loopr/)
pub fn default_data_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".loopr")
}

/// Get the default socket path (~/.loopr/daemon.sock)
pub fn default_socket_path() -> PathBuf {
    default_data_dir().join("daemon.sock")
}

/// Get the default PID file path (~/.loopr/daemon.pid)
pub fn default_pid_path() -> PathBuf {
    default_data_dir().join("daemon.pid")
}

/// Configuration for the daemon
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Path to the Unix socket
    pub socket_path: PathBuf,
    /// Path to the PID file
    pub pid_path: PathBuf,
    /// Data directory
    pub data_dir: PathBuf,
    /// Tick loop configuration
    pub tick_config: TickConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            pid_path: default_pid_path(),
            data_dir: default_data_dir(),
            tick_config: TickConfig::default(),
        }
    }
}

impl DaemonConfig {
    /// Create config with custom paths
    pub fn with_paths(socket_path: PathBuf, pid_path: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            socket_path,
            pid_path,
            data_dir,
            tick_config: TickConfig::default(),
        }
    }
}

/// Async request handler that uses DaemonContext
pub struct AsyncDaemonHandler {
    ctx: Arc<DaemonContext>,
}

impl AsyncDaemonHandler {
    /// Create a new handler with the given context
    pub fn new(ctx: Arc<DaemonContext>) -> Self {
        Self { ctx }
    }
}

impl RequestHandler for AsyncDaemonHandler {
    async fn handle(&self, request: DaemonRequest) -> DaemonResponse {
        handle_request_async(request, &self.ctx).await
    }
}

/// Handle incoming requests from clients (async version)
async fn handle_request_async(request: DaemonRequest, ctx: &DaemonContext) -> DaemonResponse {
    match request.method.as_str() {
        // Connection
        Methods::PING => DaemonResponse::success(request.id, json!({"pong": true})),

        "status" => DaemonResponse::success(
            request.id,
            json!({
                "running": true,
                "version": env!("CARGO_PKG_VERSION"),
                "llm_ready": ctx.llm_ready(),
            }),
        ),

        // Chat methods
        Methods::CHAT_SEND => handle_chat_send(request.id, &request.params, ctx).await,
        Methods::CHAT_CLEAR => handle_chat_clear(request.id, ctx).await,
        Methods::CHAT_CANCEL => handle_chat_cancel(request.id, ctx).await,

        // Loop methods
        Methods::LOOP_LIST => handle_loop_list(request.id, ctx).await,
        Methods::LOOP_GET => handle_loop_get(request.id, &request.params, ctx).await,
        Methods::LOOP_CREATE_PLAN => handle_loop_create_plan(request.id, &request.params, ctx).await,
        Methods::LOOP_START => handle_loop_start(request.id, &request.params, ctx).await,
        Methods::LOOP_PAUSE => handle_loop_pause(request.id, &request.params, ctx).await,
        Methods::LOOP_RESUME => handle_loop_resume(request.id, &request.params, ctx).await,
        Methods::LOOP_CANCEL => handle_loop_cancel(request.id, &request.params, ctx).await,
        Methods::LOOP_DELETE => handle_loop_delete(request.id, &request.params, ctx).await,

        // Plan approval methods
        Methods::PLAN_APPROVE => handle_plan_approve(request.id, &request.params, ctx).await,
        Methods::PLAN_REJECT => handle_plan_reject(request.id, &request.params, ctx).await,
        Methods::PLAN_ITERATE => handle_plan_iterate(request.id, &request.params, ctx).await,
        Methods::PLAN_GET_PREVIEW => handle_plan_get_preview(request.id, &request.params, ctx).await,

        // Metrics
        Methods::METRICS_GET => DaemonResponse::success(
            request.id,
            json!({
                "running_loops": ctx.loop_manager.read().await.running_count().await,
            }),
        ),

        // Unknown method
        _ => DaemonResponse::error(request.id, DaemonError::method_not_found(&request.method)),
    }
}

/// The main daemon struct that coordinates IPC server, signal handling, and lifecycle
pub struct Daemon {
    config: DaemonConfig,
    tick_state: Arc<RwLock<TickState>>,
}

impl Daemon {
    /// Create a new daemon with the given configuration
    pub fn new(config: DaemonConfig) -> Result<Self> {
        Ok(Self {
            config,
            tick_state: Arc::new(RwLock::new(TickState::new())),
        })
    }

    /// Create a daemon with default configuration
    pub fn with_defaults() -> Result<Self> {
        Self::new(DaemonConfig::default())
    }

    /// Check if a daemon is already running by checking the PID file
    pub fn is_running(pid_path: &Path) -> bool {
        if let Some(pid) = Self::get_pid(pid_path) {
            // Check if process exists using kill(pid, 0)
            unsafe { libc::kill(pid, 0) == 0 }
        } else {
            false
        }
    }

    /// Get the PID from the PID file if it exists
    pub fn get_pid(pid_path: &Path) -> Option<i32> {
        if !pid_path.exists() {
            return None;
        }

        let mut file = fs::File::open(pid_path).ok()?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).ok()?;
        contents.trim().parse().ok()
    }

    /// Write the current PID to the PID file
    fn write_pid(&self) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.config.pid_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let pid = std::process::id();
        let mut file = fs::File::create(&self.config.pid_path)?;
        writeln!(file, "{}", pid)?;
        Ok(())
    }

    /// Remove the PID file
    fn remove_pid(&self) {
        let _ = fs::remove_file(&self.config.pid_path);
    }

    /// Run the daemon (blocking)
    pub async fn run(&mut self) -> Result<()> {
        // Check if already running
        if Self::is_running(&self.config.pid_path) {
            return Err(LooprError::InvalidState("Daemon is already running".to_string()));
        }

        // Write PID file
        self.write_pid()?;
        info!("Daemon started with PID {}", std::process::id());

        // Ensure data directory exists
        fs::create_dir_all(&self.config.data_dir)?;

        // Create IPC server first so we can share its event channel with DaemonContext
        let server_config = IpcServerConfig::default().with_socket_path(&self.config.socket_path);
        let mut server = IpcServer::with_config(server_config);

        // Get the server's event sender to share with DaemonContext
        let event_tx = server.event_sender();

        // Create DaemonContext with the shared event channel
        let ctx = match DaemonContext::with_event_channel(&self.config.data_dir, event_tx) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                // If context creation fails (e.g., no API key), log and continue with minimal handler
                info!("Warning: Could not create full daemon context: {}", e);
                // Fall back to legacy handler
                return self.run_legacy().await;
            }
        };

        // Create async request handler
        let handler = Arc::new(AsyncDaemonHandler::new(ctx));

        info!("Daemon listening on {}", self.config.socket_path.display());

        // Run server with signal handling
        let result = tokio::select! {
            result = server.run(handler) => {
                result
            }
            _ = async {
                let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
                let mut sigint = signal(SignalKind::interrupt()).expect("Failed to register SIGINT handler");
                tokio::select! {
                    _ = sigterm.recv() => {
                        info!("Received SIGTERM, shutting down...");
                    }
                    _ = sigint.recv() => {
                        info!("Received SIGINT, shutting down...");
                    }
                }
            } => {
                info!("Signal received, stopping server...");
                let _ = server.shutdown().await;
                Ok(())
            }
        };

        // Always clean up PID file on exit
        self.remove_pid();
        info!("Daemon stopped");

        result
    }

    /// Legacy run method using synchronous handler (fallback when DaemonContext fails)
    async fn run_legacy(&mut self) -> Result<()> {
        use crate::ipc::server::CallbackHandler;

        // Create IPC server
        let server_config = IpcServerConfig::default().with_socket_path(&self.config.socket_path);
        let mut server = IpcServer::with_config(server_config);

        // Create simple request handler
        let tick_state = Arc::clone(&self.tick_state);
        let handler = Arc::new(CallbackHandler::new(move |request: DaemonRequest| {
            handle_request_sync(request, &tick_state)
        }));

        info!(
            "Daemon (legacy mode) listening on {}",
            self.config.socket_path.display()
        );

        // Run server with signal handling
        let result = tokio::select! {
            result = server.run(handler) => {
                result
            }
            _ = async {
                let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
                let mut sigint = signal(SignalKind::interrupt()).expect("Failed to register SIGINT handler");
                tokio::select! {
                    _ = sigterm.recv() => {
                        info!("Received SIGTERM, shutting down...");
                    }
                    _ = sigint.recv() => {
                        info!("Received SIGINT, shutting down...");
                    }
                }
            } => {
                info!("Signal received, stopping server...");
                let _ = server.shutdown().await;
                Ok(())
            }
        };

        // Always clean up PID file on exit
        self.remove_pid();
        info!("Daemon stopped");

        result
    }

    /// Stop a running daemon by sending SIGTERM
    pub fn stop(pid_path: &Path) -> Result<bool> {
        if let Some(pid) = Self::get_pid(pid_path) {
            info!("Sending SIGTERM to daemon (PID {})", pid);

            // Send SIGTERM
            let result = unsafe { libc::kill(pid, libc::SIGTERM) };
            if result != 0 {
                return Err(LooprError::Ipc(format!("Failed to send SIGTERM to PID {}", pid)));
            }

            // Wait for process to exit (up to 3 seconds)
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if unsafe { libc::kill(pid, 0) } != 0 {
                    // Process has exited
                    // Clean up stale PID file if it still exists
                    let _ = fs::remove_file(pid_path);
                    return Ok(true);
                }
            }

            // Process didn't exit, send SIGKILL
            info!("Daemon did not stop, sending SIGKILL");
            let result = unsafe { libc::kill(pid, libc::SIGKILL) };
            if result != 0 {
                return Err(LooprError::Ipc(format!("Failed to send SIGKILL to PID {}", pid)));
            }

            // Clean up PID file
            let _ = fs::remove_file(pid_path);
            Ok(true)
        } else {
            Ok(false) // No daemon running
        }
    }
}

/// Handle incoming requests from clients (sync version for fallback)
fn handle_request_sync(request: DaemonRequest, _tick_state: &Arc<RwLock<TickState>>) -> DaemonResponse {
    match request.method.as_str() {
        "ping" => DaemonResponse::success(request.id, json!({"pong": true})),

        "status" => DaemonResponse::success(
            request.id,
            json!({
                "running": true,
                "version": env!("CARGO_PKG_VERSION"),
                "mode": "legacy",
            }),
        ),

        "loop.list" => DaemonResponse::success(request.id, json!({"loops": []})),

        "loop.get" => DaemonResponse::success(request.id, json!({"loop": null})),

        "loop.create_plan" => DaemonResponse::success(request.id, json!({"id": "plan-001", "status": "created"})),

        _ => DaemonResponse::error(request.id, DaemonError::method_not_found(&request.method)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_default_paths() {
        let data_dir = default_data_dir();
        assert!(data_dir.ends_with(".loopr"));

        let socket_path = default_socket_path();
        assert!(socket_path.ends_with("daemon.sock"));

        let pid_path = default_pid_path();
        assert!(pid_path.ends_with("daemon.pid"));
    }

    #[test]
    fn test_daemon_config_default() {
        let config = DaemonConfig::default();
        assert!(config.socket_path.ends_with("daemon.sock"));
        assert!(config.pid_path.ends_with("daemon.pid"));
        assert!(config.data_dir.ends_with(".loopr"));
    }

    #[test]
    fn test_daemon_config_with_paths() {
        let config = DaemonConfig::with_paths(
            PathBuf::from("/tmp/test.sock"),
            PathBuf::from("/tmp/test.pid"),
            PathBuf::from("/tmp/data"),
        );
        assert_eq!(config.socket_path, PathBuf::from("/tmp/test.sock"));
        assert_eq!(config.pid_path, PathBuf::from("/tmp/test.pid"));
        assert_eq!(config.data_dir, PathBuf::from("/tmp/data"));
    }

    #[test]
    fn test_daemon_new() {
        let config = DaemonConfig::default();
        let daemon = Daemon::new(config);
        assert!(daemon.is_ok());
    }

    #[test]
    fn test_daemon_is_running_no_pid_file() {
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("nonexistent.pid");
        assert!(!Daemon::is_running(&pid_path));
    }

    #[test]
    fn test_daemon_get_pid_no_file() {
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("nonexistent.pid");
        assert!(Daemon::get_pid(&pid_path).is_none());
    }

    #[test]
    fn test_daemon_get_pid_with_file() {
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        fs::write(&pid_path, "12345\n").unwrap();
        assert_eq!(Daemon::get_pid(&pid_path), Some(12345));
    }

    #[test]
    fn test_daemon_get_pid_invalid_content() {
        let dir = tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");
        fs::write(&pid_path, "not-a-number\n").unwrap();
        assert!(Daemon::get_pid(&pid_path).is_none());
    }

    #[test]
    fn test_handle_request_sync_ping() {
        let tick_state = Arc::new(RwLock::new(TickState::new()));
        let request = DaemonRequest::new(1, "ping", json!({}));
        let response = handle_request_sync(request, &tick_state);
        assert!(response.is_success());
        assert!(response.result.unwrap()["pong"].as_bool().unwrap());
    }

    #[test]
    fn test_handle_request_sync_status() {
        let tick_state = Arc::new(RwLock::new(TickState::new()));
        let request = DaemonRequest::new(2, "status", json!({}));
        let response = handle_request_sync(request, &tick_state);
        assert!(response.is_success());
        assert!(response.result.unwrap()["running"].as_bool().unwrap());
    }

    #[test]
    fn test_handle_request_sync_unknown_method() {
        let tick_state = Arc::new(RwLock::new(TickState::new()));
        let request = DaemonRequest::new(3, "unknown.method", json!({}));
        let response = handle_request_sync(request, &tick_state);
        assert!(!response.is_success());
        assert!(response.error.is_some());
    }
}
