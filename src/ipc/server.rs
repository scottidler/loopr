//! IPC Server - Unix socket server for TUI-daemon communication
//!
//! Provides:
//! - Unix stream socket listener
//! - Client connection handling
//! - Request routing and response sending
//! - Event broadcasting to subscribers

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::error::{LooprError, Result};
use crate::ipc::messages::{DaemonEvent, DaemonRequest, DaemonResponse, DaemonError, ErrorCode};

/// Configuration for the IPC server
#[derive(Debug, Clone)]
pub struct IpcServerConfig {
    /// Path to the Unix socket
    pub socket_path: PathBuf,
    /// Maximum number of concurrent clients
    pub max_clients: usize,
    /// Channel capacity for events
    pub event_channel_capacity: usize,
}

impl Default for IpcServerConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/tmp/loopr-daemon.sock"),
            max_clients: 16,
            event_channel_capacity: 256,
        }
    }
}

impl IpcServerConfig {
    /// Create config with custom socket path
    pub fn with_socket_path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.socket_path = path.as_ref().to_path_buf();
        self
    }

    /// Set max clients
    pub fn with_max_clients(mut self, max: usize) -> Self {
        self.max_clients = max;
        self
    }
}

/// Handler trait for processing requests
pub trait RequestHandler: Send + Sync {
    /// Handle a request and return a response
    fn handle(
        &self,
        request: DaemonRequest,
    ) -> impl std::future::Future<Output = DaemonResponse> + Send;
}

/// Simple handler that routes to a callback
pub struct CallbackHandler<F>
where
    F: Fn(DaemonRequest) -> DaemonResponse + Send + Sync,
{
    callback: F,
}

impl<F> CallbackHandler<F>
where
    F: Fn(DaemonRequest) -> DaemonResponse + Send + Sync,
{
    pub fn new(callback: F) -> Self {
        Self { callback }
    }
}

impl<F> RequestHandler for CallbackHandler<F>
where
    F: Fn(DaemonRequest) -> DaemonResponse + Send + Sync,
{
    fn handle(
        &self,
        request: DaemonRequest,
    ) -> impl std::future::Future<Output = DaemonResponse> + Send {
        let result = (self.callback)(request);
        async move { result }
    }
}

/// Connected client state
#[derive(Debug)]
struct ClientState {
    /// Unique client ID (used for debugging/logging)
    #[allow(dead_code)]
    id: u64,
    /// Whether client is subscribed to events
    subscribed: bool,
}

/// IPC Server for daemon communication
pub struct IpcServer {
    config: IpcServerConfig,
    /// Connected clients
    clients: Arc<RwLock<HashMap<u64, ClientState>>>,
    /// Event broadcaster
    event_tx: broadcast::Sender<DaemonEvent>,
    /// Next client ID
    next_client_id: Arc<RwLock<u64>>,
    /// Shutdown signal
    shutdown_tx: Option<mpsc::Sender<()>>,
}

impl IpcServer {
    /// Create a new IPC server with default config
    pub fn new() -> Self {
        Self::with_config(IpcServerConfig::default())
    }

    /// Create a new IPC server with custom config
    pub fn with_config(config: IpcServerConfig) -> Self {
        let (event_tx, _) = broadcast::channel(config.event_channel_capacity);
        Self {
            config,
            clients: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            next_client_id: Arc::new(RwLock::new(1)),
            shutdown_tx: None,
        }
    }

    /// Get the socket path
    pub fn socket_path(&self) -> &Path {
        &self.config.socket_path
    }

    /// Broadcast an event to all subscribed clients
    pub fn broadcast(&self, event: DaemonEvent) -> Result<usize> {
        match self.event_tx.send(event) {
            Ok(count) => Ok(count),
            Err(_) => Ok(0), // No receivers
        }
    }

    /// Get count of connected clients
    pub async fn client_count(&self) -> usize {
        self.clients.read().await.len()
    }

    /// Run the server with a request handler
    pub async fn run<H: RequestHandler + 'static>(
        &mut self,
        handler: Arc<H>,
    ) -> Result<()> {
        // Remove existing socket if present
        if self.config.socket_path.exists() {
            std::fs::remove_file(&self.config.socket_path)?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.config.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&self.config.socket_path)
            .map_err(|e| LooprError::Ipc(format!("Failed to bind socket: {}", e)))?;

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        self.shutdown_tx = Some(shutdown_tx);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let client_count = self.clients.read().await.len();
                            if client_count >= self.config.max_clients {
                                // Reject connection - at capacity
                                continue;
                            }

                            let client_id = {
                                let mut id = self.next_client_id.write().await;
                                let current = *id;
                                *id += 1;
                                current
                            };

                            // Register client
                            {
                                let mut clients = self.clients.write().await;
                                clients.insert(client_id, ClientState {
                                    id: client_id,
                                    subscribed: false,
                                });
                            }

                            // Spawn client handler
                            let handler_clone = Arc::clone(&handler);
                            let clients = Arc::clone(&self.clients);
                            let event_rx = self.event_tx.subscribe();

                            tokio::spawn(async move {
                                let _ = handle_client(
                                    stream,
                                    client_id,
                                    handler_clone,
                                    clients,
                                    event_rx,
                                ).await;
                            });
                        }
                        Err(e) => {
                            eprintln!("Accept error: {}", e);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    break;
                }
            }
        }

        // Cleanup socket
        let _ = std::fs::remove_file(&self.config.socket_path);
        Ok(())
    }

    /// Signal the server to shutdown
    pub async fn shutdown(&self) -> Result<()> {
        if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(()).await;
        }
        Ok(())
    }
}

impl Default for IpcServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle a single client connection
async fn handle_client<H: RequestHandler>(
    stream: UnixStream,
    client_id: u64,
    handler: Arc<H>,
    clients: Arc<RwLock<HashMap<u64, ClientState>>>,
    mut event_rx: broadcast::Receiver<DaemonEvent>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        tokio::select! {
            // Handle incoming requests
            read_result = reader.read_line(&mut line) => {
                match read_result {
                    Ok(0) => break, // EOF - client disconnected
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            line.clear();
                            continue;
                        }

                        match serde_json::from_str::<DaemonRequest>(trimmed) {
                            Ok(request) => {
                                // Check for subscribe method
                                if request.method == "subscribe" {
                                    let mut clients = clients.write().await;
                                    if let Some(state) = clients.get_mut(&client_id) {
                                        state.subscribed = true;
                                    }
                                    let response = DaemonResponse::success(request.id, serde_json::json!({"subscribed": true}));
                                    let response_json = serde_json::to_string(&response).unwrap_or_default();
                                    let _ = writer.write_all(response_json.as_bytes()).await;
                                    let _ = writer.write_all(b"\n").await;
                                } else {
                                    let response = handler.handle(request).await;
                                    let response_json = serde_json::to_string(&response).unwrap_or_default();
                                    let _ = writer.write_all(response_json.as_bytes()).await;
                                    let _ = writer.write_all(b"\n").await;
                                }
                            }
                            Err(e) => {
                                let response = DaemonResponse::error(
                                    0,
                                    DaemonError::new(ErrorCode::PARSE_ERROR, format!("Parse error: {}", e)),
                                );
                                let response_json = serde_json::to_string(&response).unwrap_or_default();
                                let _ = writer.write_all(response_json.as_bytes()).await;
                                let _ = writer.write_all(b"\n").await;
                            }
                        }
                        line.clear();
                    }
                    Err(_) => break,
                }
            }
            // Forward events to subscribed clients
            event_result = event_rx.recv() => {
                match event_result {
                    Ok(event) => {
                        let is_subscribed = {
                            let clients = clients.read().await;
                            clients.get(&client_id).is_some_and(|s| s.subscribed)
                        };
                        if is_subscribed {
                            let event_json = serde_json::to_string(&event).unwrap_or_default();
                            if writer.write_all(event_json.as_bytes()).await.is_err() {
                                break;
                            }
                            if writer.write_all(b"\n").await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Client lagged behind, continue
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        }
    }

    // Remove client on disconnect
    {
        let mut clients = clients.write().await;
        clients.remove(&client_id);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    struct EchoHandler;

    impl RequestHandler for EchoHandler {
        fn handle(
            &self,
            request: DaemonRequest,
        ) -> impl std::future::Future<Output = DaemonResponse> + Send {
            async move { DaemonResponse::success(request.id, request.params) }
        }
    }

    #[test]
    fn test_server_config_default() {
        let config = IpcServerConfig::default();
        assert_eq!(config.max_clients, 16);
        assert_eq!(config.event_channel_capacity, 256);
    }

    #[test]
    fn test_server_config_builder() {
        let config = IpcServerConfig::default()
            .with_socket_path("/tmp/test.sock")
            .with_max_clients(32);
        assert_eq!(config.socket_path, PathBuf::from("/tmp/test.sock"));
        assert_eq!(config.max_clients, 32);
    }

    #[test]
    fn test_server_new() {
        let server = IpcServer::new();
        assert_eq!(server.config.max_clients, 16);
    }

    #[test]
    fn test_server_with_config() {
        let config = IpcServerConfig::default().with_max_clients(8);
        let server = IpcServer::with_config(config);
        assert_eq!(server.config.max_clients, 8);
    }

    #[test]
    fn test_server_socket_path() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        let config = IpcServerConfig::default().with_socket_path(&socket_path);
        let server = IpcServer::with_config(config);
        assert_eq!(server.socket_path(), socket_path);
    }

    #[tokio::test]
    async fn test_server_client_count_initial() {
        let server = IpcServer::new();
        assert_eq!(server.client_count().await, 0);
    }

    #[test]
    fn test_broadcast_no_receivers() {
        let server = IpcServer::new();
        let event = DaemonEvent::new("test", serde_json::json!({}));
        let result = server.broadcast(event);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_callback_handler() {
        let handler = CallbackHandler::new(|req| {
            DaemonResponse::success(req.id, serde_json::json!({"echo": true}))
        });
        let request = DaemonRequest::new(1, "test", serde_json::json!({}));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let response = rt.block_on(handler.handle(request));
        assert!(response.result.is_some());
    }

    #[test]
    fn test_client_state() {
        let state = ClientState {
            id: 1,
            subscribed: false,
        };
        assert_eq!(state.id, 1);
        assert!(!state.subscribed);
    }

    #[tokio::test]
    async fn test_server_shutdown() {
        let server = IpcServer::new();
        let result = server.shutdown().await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_echo_handler() {
        let handler = EchoHandler;
        let request = DaemonRequest::new(42, "echo", serde_json::json!({"data": "test"}));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let response = rt.block_on(handler.handle(request));
        assert_eq!(response.id, 42);
        assert!(response.result.is_some());
        assert!(response.error.is_none());
    }

    #[tokio::test]
    async fn test_server_multiple_broadcasts() {
        let server = IpcServer::new();

        // Subscribe to events
        let mut rx = server.event_tx.subscribe();

        // Broadcast events
        let event1 = DaemonEvent::new("event1", serde_json::json!({"n": 1}));
        let event2 = DaemonEvent::new("event2", serde_json::json!({"n": 2}));

        server.broadcast(event1).unwrap();
        server.broadcast(event2).unwrap();

        // Receive events
        let received1 = rx.recv().await.unwrap();
        let received2 = rx.recv().await.unwrap();

        assert_eq!(received1.event, "event1");
        assert_eq!(received2.event, "event2");
    }

    #[test]
    fn test_default_impl() {
        let server = IpcServer::default();
        assert_eq!(server.config.max_clients, 16);
    }

    #[tokio::test]
    async fn test_server_next_client_id() {
        let server = IpcServer::new();

        let id1 = {
            let mut id = server.next_client_id.write().await;
            let current = *id;
            *id += 1;
            current
        };

        let id2 = {
            let mut id = server.next_client_id.write().await;
            let current = *id;
            *id += 1;
            current
        };

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn test_daemon_event_serialization() {
        let event = DaemonEvent::new("loop.created", serde_json::json!({"id": "test-123"}));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("loop.created"));
        assert!(json.contains("test-123"));
    }

    #[test]
    fn test_daemon_request_parsing() {
        let json = r#"{"id":1,"method":"loop.list","params":{}}"#;
        let request: DaemonRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.id, 1);
        assert_eq!(request.method, "loop.list");
    }

    #[test]
    fn test_daemon_response_success() {
        let response = DaemonResponse::success(1, serde_json::json!({"status": "ok"}));
        assert_eq!(response.id, 1);
        assert!(response.result.is_some());
        assert!(response.error.is_none());
    }

    #[test]
    fn test_daemon_response_error() {
        let error = DaemonError::new(ErrorCode::LOOP_NOT_FOUND, "Loop xyz not found");
        let response = DaemonResponse::error(2, error);
        assert_eq!(response.id, 2);
        assert!(response.result.is_none());
        assert!(response.error.is_some());
        assert_eq!(response.error.as_ref().unwrap().code, ErrorCode::LOOP_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_server_config_event_capacity() {
        let config = IpcServerConfig {
            socket_path: PathBuf::from("/tmp/test.sock"),
            max_clients: 4,
            event_channel_capacity: 64,
        };
        let server = IpcServer::with_config(config);
        assert_eq!(server.config.event_channel_capacity, 64);
    }

    #[test]
    fn test_parse_error_response() {
        let error = DaemonError::new(ErrorCode::PARSE_ERROR, "Invalid JSON");
        let response = DaemonResponse::error(0, error);
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("-32700"));
        assert!(json.contains("Invalid JSON"));
    }
}
