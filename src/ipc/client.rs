//! IPC client for TUI to communicate with daemon.
//!
//! Provides async connection to daemon Unix socket with:
//! - Request/response communication
//! - Event subscription and streaming
//! - Automatic reconnection support

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::daemon::default_socket_path;
use crate::error::{LooprError, Result};
use crate::ipc::messages::{DaemonError, DaemonEvent, DaemonRequest, DaemonResponse};

/// Configuration for IPC client.
#[derive(Debug, Clone)]
pub struct IpcClientConfig {
    /// Path to daemon Unix socket.
    pub socket_path: PathBuf,
    /// Request timeout in milliseconds.
    pub request_timeout_ms: u64,
    /// Whether to auto-reconnect on disconnect.
    pub auto_reconnect: bool,
}

impl Default for IpcClientConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            request_timeout_ms: 30000,
            auto_reconnect: true,
        }
    }
}

impl IpcClientConfig {
    /// Create config with custom socket path.
    pub fn with_socket(path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: path.into(),
            ..Default::default()
        }
    }
}

/// Pending request awaiting response.
struct PendingRequest {
    sender: oneshot::Sender<std::result::Result<DaemonResponse, DaemonError>>,
}

/// IPC client for communicating with daemon.
pub struct IpcClient {
    config: IpcClientConfig,
    writer: Arc<Mutex<Option<tokio::io::WriteHalf<UnixStream>>>>,
    pending: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    next_id: AtomicU64,
    connected: AtomicBool,
    event_sender: mpsc::Sender<DaemonEvent>,
    event_receiver: Mutex<mpsc::Receiver<DaemonEvent>>,
}

impl IpcClient {
    /// Create a new IPC client with config.
    pub fn new(config: IpcClientConfig) -> Self {
        let (event_sender, event_receiver) = mpsc::channel(100);
        Self {
            config,
            writer: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            connected: AtomicBool::new(false),
            event_sender,
            event_receiver: Mutex::new(event_receiver),
        }
    }

    /// Create client with default config.
    pub fn with_default_config() -> Self {
        Self::new(IpcClientConfig::default())
    }

    /// Create client with socket path.
    pub fn with_socket(path: impl Into<PathBuf>) -> Self {
        Self::new(IpcClientConfig::with_socket(path))
    }

    /// Connect to daemon.
    pub async fn connect(&self) -> Result<()> {
        let stream = UnixStream::connect(&self.config.socket_path)
            .await
            .map_err(|e| LooprError::Ipc(format!("Failed to connect: {}", e)))?;

        let (reader, writer) = tokio::io::split(stream);

        // Store writer
        {
            let mut w = self.writer.lock().await;
            *w = Some(writer);
        }
        self.connected.store(true, Ordering::SeqCst);

        // Spawn reader task
        let pending = Arc::clone(&self.pending);
        let event_sender = self.event_sender.clone();
        let connected = Arc::new(AtomicBool::new(true));
        let connected_clone = Arc::clone(&connected);

        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();

            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        // EOF - connection closed
                        connected_clone.store(false, Ordering::SeqCst);
                        break;
                    }
                    Ok(_) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }

                        // Try to parse as response first (has "id" field)
                        if let Ok(response) = serde_json::from_str::<DaemonResponse>(line) {
                            let mut pending_guard = pending.lock().await;
                            if let Some(req) = pending_guard.remove(&response.id) {
                                let _ = req.sender.send(Ok(response));
                            }
                        }
                        // Try to parse as event (has "event" field)
                        else if let Ok(event) = serde_json::from_str::<DaemonEvent>(line) {
                            let _ = event_sender.send(event).await;
                        }
                        // Unknown message type - ignore
                    }
                    Err(_) => {
                        connected_clone.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    /// Disconnect from daemon.
    pub async fn disconnect(&self) -> Result<()> {
        let mut writer = self.writer.lock().await;
        *writer = None;
        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Check if connected.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Get socket path.
    pub fn socket_path(&self) -> &Path {
        &self.config.socket_path
    }

    /// Send a request and wait for response.
    pub async fn request(&self, method: &str, params: serde_json::Value) -> Result<DaemonResponse> {
        if !self.is_connected() {
            return Err(LooprError::Ipc("Not connected".into()));
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = DaemonRequest::new(id, method, params);

        // Create response channel
        let (tx, rx) = oneshot::channel();

        // Register pending request
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, PendingRequest { sender: tx });
        }

        // Send request
        {
            let mut writer = self.writer.lock().await;
            if let Some(w) = writer.as_mut() {
                let json = serde_json::to_string(&request)
                    .map_err(|e| LooprError::Ipc(format!("Failed to serialize: {}", e)))?;
                w.write_all(json.as_bytes())
                    .await
                    .map_err(|e| LooprError::Ipc(format!("Failed to write: {}", e)))?;
                w.write_all(b"\n")
                    .await
                    .map_err(|e| LooprError::Ipc(format!("Failed to write newline: {}", e)))?;
                w.flush()
                    .await
                    .map_err(|e| LooprError::Ipc(format!("Failed to flush: {}", e)))?;
            } else {
                // Remove pending request
                let mut pending = self.pending.lock().await;
                pending.remove(&id);
                return Err(LooprError::Ipc("Writer not available".into()));
            }
        }

        // Wait for response with timeout
        let timeout = tokio::time::Duration::from_millis(self.config.request_timeout_ms);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(response))) => Ok(response),
            Ok(Ok(Err(err))) => Err(LooprError::Ipc(err.message)),
            Ok(Err(_)) => Err(LooprError::Ipc("Response channel closed".into())),
            Err(_) => {
                // Timeout - remove pending request
                let mut pending = self.pending.lock().await;
                pending.remove(&id);
                Err(LooprError::Ipc("Request timeout".into()))
            }
        }
    }

    /// Send a request with no parameters.
    pub async fn request_no_params(&self, method: &str) -> Result<DaemonResponse> {
        self.request(method, serde_json::json!({})).await
    }

    /// Receive next event (blocks until event available).
    pub async fn recv_event(&self) -> Option<DaemonEvent> {
        let mut receiver = self.event_receiver.lock().await;
        receiver.recv().await
    }

    /// Try to receive event without blocking.
    pub async fn try_recv_event(&self) -> Option<DaemonEvent> {
        let mut receiver = self.event_receiver.lock().await;
        receiver.try_recv().ok()
    }

    // Convenience methods for common operations

    /// Send ping request.
    pub async fn ping(&self) -> Result<bool> {
        let response = self.request_no_params("ping").await?;
        Ok(response.is_success())
    }

    /// List all loops.
    pub async fn list_loops(&self) -> Result<DaemonResponse> {
        self.request_no_params("loop.list").await
    }

    /// Get loop by ID.
    pub async fn get_loop(&self, id: &str) -> Result<DaemonResponse> {
        self.request("loop.get", serde_json::json!({ "id": id })).await
    }

    /// Create a plan.
    pub async fn create_plan(&self, description: &str) -> Result<DaemonResponse> {
        self.request("loop.create_plan", serde_json::json!({ "description": description }))
            .await
    }

    /// Approve a plan.
    pub async fn approve_plan(&self, id: &str) -> Result<DaemonResponse> {
        self.request("plan.approve", serde_json::json!({ "id": id })).await
    }

    /// Reject a plan.
    pub async fn reject_plan(&self, id: &str, reason: Option<&str>) -> Result<DaemonResponse> {
        self.request("plan.reject", serde_json::json!({ "id": id, "reason": reason }))
            .await
    }

    /// Iterate on a plan with feedback.
    pub async fn iterate_plan(&self, id: &str, feedback: &str) -> Result<DaemonResponse> {
        self.request("plan.iterate", serde_json::json!({ "id": id, "feedback": feedback }))
            .await
    }

    /// Pause a loop.
    pub async fn pause_loop(&self, id: &str) -> Result<DaemonResponse> {
        self.request("loop.pause", serde_json::json!({ "id": id })).await
    }

    /// Resume a loop.
    pub async fn resume_loop(&self, id: &str) -> Result<DaemonResponse> {
        self.request("loop.resume", serde_json::json!({ "id": id })).await
    }

    /// Cancel/stop a loop.
    pub async fn cancel_loop(&self, id: &str) -> Result<DaemonResponse> {
        self.request("loop.cancel", serde_json::json!({ "id": id })).await
    }

    /// Send chat message.
    pub async fn chat_send(&self, message: &str) -> Result<DaemonResponse> {
        self.request("chat.send", serde_json::json!({ "message": message }))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = IpcClientConfig::default();
        assert!(config.socket_path.ends_with("daemon.sock"));
        assert_eq!(config.request_timeout_ms, 30000);
        assert!(config.auto_reconnect);
    }

    #[test]
    fn test_config_with_socket() {
        let config = IpcClientConfig::with_socket("/custom/path.sock");
        assert_eq!(config.socket_path, PathBuf::from("/custom/path.sock"));
    }

    #[test]
    fn test_client_new() {
        let client = IpcClient::with_default_config();
        assert!(!client.is_connected());
        assert!(client.socket_path().ends_with("daemon.sock"));
    }

    #[test]
    fn test_client_with_socket() {
        let client = IpcClient::with_socket("/test/socket.sock");
        assert_eq!(client.socket_path(), Path::new("/test/socket.sock"));
    }

    #[test]
    fn test_next_id_increments() {
        let client = IpcClient::with_default_config();
        let id1 = client.next_id.fetch_add(1, Ordering::SeqCst);
        let id2 = client.next_id.fetch_add(1, Ordering::SeqCst);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[tokio::test]
    async fn test_not_connected_error() {
        let client = IpcClient::with_default_config();
        let result = client.request_no_params("ping").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, LooprError::Ipc(_)));
    }

    #[tokio::test]
    async fn test_disconnect_clears_writer() {
        let client = IpcClient::with_default_config();
        client.disconnect().await.unwrap();
        assert!(!client.is_connected());
    }

    #[tokio::test]
    async fn test_try_recv_event_empty() {
        let client = IpcClient::with_default_config();
        let event = client.try_recv_event().await;
        assert!(event.is_none());
    }

    // Integration tests requiring actual socket connection would go here
    // For unit tests, we verify the client structure and error handling

    #[test]
    fn test_pending_request_channel() {
        let (tx, _rx) = oneshot::channel();
        let _pending = PendingRequest { sender: tx };
        // Just verifying the type compiles correctly
    }

    #[tokio::test]
    async fn test_connect_nonexistent_socket() {
        let client = IpcClient::with_socket("/nonexistent/path/socket.sock");
        let result = client.connect().await;
        assert!(result.is_err());
    }

    #[test]
    fn test_config_clone() {
        let config = IpcClientConfig::default();
        let cloned = config.clone();
        assert_eq!(cloned.socket_path, config.socket_path);
        assert_eq!(cloned.request_timeout_ms, config.request_timeout_ms);
    }

    #[test]
    fn test_config_debug() {
        let config = IpcClientConfig::default();
        let debug = format!("{:?}", config);
        assert!(debug.contains("socket_path"));
    }

    #[tokio::test]
    async fn test_request_builds_correct_json() {
        // We can't send without connection, but we can verify DaemonRequest construction
        let request = DaemonRequest::new(1, "test.method", serde_json::json!({"key": "value"}));
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"method\":\"test.method\""));
        assert!(json.contains("\"key\":\"value\""));
    }

    #[test]
    fn test_daemon_response_parsing() {
        let json = r#"{"id":1,"result":{"status":"ok"}}"#;
        let response: DaemonResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, 1);
        assert!(response.is_success());
    }

    #[test]
    fn test_daemon_event_parsing() {
        let json = r#"{"event":"loop.created","data":{"id":"test-123"}}"#;
        let event: DaemonEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event, "loop.created");
        assert_eq!(event.data["id"], "test-123");
    }

    #[tokio::test]
    async fn test_convenience_methods_require_connection() {
        let client = IpcClient::with_default_config();

        // All convenience methods should fail when not connected
        assert!(client.ping().await.is_err());
        assert!(client.list_loops().await.is_err());
        assert!(client.get_loop("test").await.is_err());
        assert!(client.create_plan("task").await.is_err());
        assert!(client.approve_plan("id").await.is_err());
        assert!(client.reject_plan("id", None).await.is_err());
        assert!(client.iterate_plan("id", "feedback").await.is_err());
        assert!(client.pause_loop("id").await.is_err());
        assert!(client.resume_loop("id").await.is_err());
        assert!(client.cancel_loop("id").await.is_err());
        assert!(client.chat_send("hello").await.is_err());
    }

    #[test]
    fn test_error_message_contains_context() {
        let error = LooprError::Ipc("Test error".into());
        let msg = format!("{}", error);
        assert!(msg.contains("Test error"));
    }
}
