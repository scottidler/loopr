use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::SinkExt;
use tokio::net::UnixStream;
use tokio_stream::StreamExt;
use tokio_util::codec::{Framed, LinesCodec};

use super::codec::ndjson_codec;
use super::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, IpcMessage};

/// Unix socket IPC client for connecting to the daemon.
/// Used by the TUI and CLI to send requests and receive responses/events.
pub struct IpcClient {
    framed: Framed<UnixStream, LinesCodec>,
    next_id: AtomicU64,
}

impl IpcClient {
    /// Connect to the daemon at the given socket path.
    pub async fn connect(socket_path: impl AsRef<Path>) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        Ok(Self {
            framed: Framed::new(stream, ndjson_codec()),
            next_id: AtomicU64::new(1),
        })
    }

    /// Send a request and wait for the matching response.
    /// Any events received while waiting are collected and returned alongside the response.
    pub async fn request(
        &mut self,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<(DaemonResponse, Vec<DaemonEvent>), ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = DaemonRequest::new(id, method, params);

        // Send the request
        let line = serde_json::to_string(&req).map_err(ClientError::Serialize)?;
        self.framed
            .send(line)
            .await
            .map_err(|e| ClientError::Io(std::io::Error::other(e.to_string())))?;

        // Read messages until we get the response matching our request id
        let mut events = Vec::new();
        loop {
            let line = self
                .framed
                .next()
                .await
                .ok_or(ClientError::Disconnected)?
                .map_err(|e| ClientError::Io(std::io::Error::other(e.to_string())))?;

            match IpcMessage::from_json(&line).map_err(ClientError::Deserialize)? {
                IpcMessage::Response(resp) if resp.id == id => {
                    return Ok((resp, events));
                }
                IpcMessage::Response(_) => {
                    // Response for a different request id — skip (shouldn't happen with sequential requests)
                }
                IpcMessage::Event(event) => {
                    events.push(event);
                }
            }
        }
    }

    /// Send a fire-and-forget request (no response expected — but still assigns an id).
    pub async fn send(&mut self, method: impl Into<String>, params: serde_json::Value) -> Result<u64, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = DaemonRequest::new(id, method, params);
        let line = serde_json::to_string(&req).map_err(ClientError::Serialize)?;
        self.framed
            .send(line)
            .await
            .map_err(|e| ClientError::Io(std::io::Error::other(e.to_string())))?;
        Ok(id)
    }

    /// Read the next message from the daemon (response or event).
    /// Returns None if the connection is closed.
    pub async fn recv(&mut self) -> Result<Option<IpcMessage>, ClientError> {
        match self.framed.next().await {
            Some(Ok(line)) => {
                let msg = IpcMessage::from_json(&line).map_err(ClientError::Deserialize)?;
                Ok(Some(msg))
            }
            Some(Err(e)) => Err(ClientError::Io(std::io::Error::other(e.to_string()))),
            None => Ok(None),
        }
    }

    /// Perform the system.handshake with the daemon.
    pub async fn handshake(&mut self, client_version: &str) -> Result<DaemonResponse, ClientError> {
        let (resp, _events) = self
            .request(
                "system.handshake",
                serde_json::json!({ "client_version": client_version }),
            )
            .await?;
        Ok(resp)
    }
}

/// Errors that can occur during IPC client operations.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("I/O error: {0}")]
    Io(#[source] std::io::Error),

    #[error("serialization error: {0}")]
    Serialize(#[source] serde_json::Error),

    #[error("deserialization error: {0}")]
    Deserialize(#[source] serde_json::Error),

    #[error("disconnected from daemon")]
    Disconnected,
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::server::{IpcServer, handle_client};
    use serde_json::json;
    use std::path::PathBuf;

    fn temp_socket_path() -> PathBuf {
        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("client-{}.sock", crate::id::generate_id("xx")))
    }

    /// Helper: start a server that echoes the method back.
    async fn start_echo_server(
        path: &PathBuf,
    ) -> (tokio::task::JoinHandle<()>, tokio::sync::broadcast::Sender<DaemonEvent>) {
        let server = IpcServer::new(path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let handle = tokio::spawn(async move {
            // Accept one client
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(
                    stream,
                    |req| DaemonResponse::ok(req.id, json!({"echo": req.method})),
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        (handle, tx)
    }

    #[tokio::test]
    async fn test_client_connect_and_request() {
        let path = temp_socket_path();
        let (server_handle, _tx) = start_echo_server(&path).await;

        // Small delay for server to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        let (resp, events) = client.request("plan.create", json!({"title": "test"})).await.unwrap();

        assert_eq!(resp.id, 1);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["echo"], "plan.create");
        assert!(events.is_empty());

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_client_multiple_requests() {
        let path = temp_socket_path();
        let (server_handle, _tx) = start_echo_server(&path).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();

        for i in 1..=3u64 {
            let method = format!("test.method_{i}");
            let (resp, _) = client.request(&method, json!(null)).await.unwrap();
            assert_eq!(resp.id, i);
            assert_eq!(resp.result.unwrap()["echo"], method);
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_client_handshake() {
        let path = temp_socket_path();
        let (server_handle, _tx) = start_echo_server(&path).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        let resp = client.handshake("0.1.0").await.unwrap();

        assert_eq!(resp.id, 1);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["echo"], "system.handshake");

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_client_receives_events_during_request() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_rx = tx.subscribe();

        // Server that broadcasts an event before responding
        let tx_clone = tx.clone();
        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // Use a handler that triggers an event broadcast
                handle_client(
                    stream,
                    move |req| {
                        // Broadcast an event before responding
                        let _ = tx_clone.send(DaemonEvent::record_created("plan", "p1"));
                        DaemonResponse::ok(req.id, json!({"created": true}))
                    },
                    event_rx,
                )
                .await;
            }
            server.cleanup();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();
        let (resp, events) = client.request("plan.create", json!({"title": "test"})).await.unwrap();

        assert!(!resp.is_error());
        // The event may or may not arrive before the response depending on timing.
        // We just verify the response is correct.
        assert_eq!(resp.result.unwrap()["created"], true);
        // Events collected during request wait (if any)
        for event in &events {
            assert_eq!(event.event, "record.created");
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_client_recv() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<DaemonEvent>(16);
        let event_tx = tx.clone();

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let event_rx = event_tx.subscribe();
                handle_client(stream, |req| DaemonResponse::ok(req.id, json!(null)), event_rx).await;
            }
            server.cleanup();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();

        // Send a request, then recv the response
        let id = client.send("system.status", json!(null)).await.unwrap();
        assert_eq!(id, 1);

        let msg = client.recv().await.unwrap().unwrap();
        match msg {
            IpcMessage::Response(resp) => {
                assert_eq!(resp.id, 1);
                assert!(!resp.is_error());
            }
            _ => panic!("expected Response, got Event"),
        }

        // Broadcast an event and recv it
        tx.send(DaemonEvent::record_created("plan", "p1")).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let msg = client.recv().await.unwrap().unwrap();
        match msg {
            IpcMessage::Event(event) => {
                assert_eq!(event.event, "record.created");
            }
            _ => panic!("expected Event, got Response"),
        }

        drop(client);
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_client_disconnected() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();

        // Server that immediately closes after accepting
        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                drop(stream); // close immediately
            }
            server.cleanup();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();

        // recv should return None when server disconnects
        let result = client.recv().await.unwrap();
        assert!(result.is_none());

        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn test_client_connect_failure() {
        let result = IpcClient::connect("/tmp/nonexistent-loopr-socket.sock").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_client_error_display() {
        let err = ClientError::Disconnected;
        assert_eq!(err.to_string(), "disconnected from daemon");

        let io_err = ClientError::Io(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"));
        assert!(io_err.to_string().contains("I/O error"));
    }

    #[tokio::test]
    async fn test_client_send_returns_incrementing_ids() {
        let path = temp_socket_path();
        let (server_handle, _tx) = start_echo_server(&path).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = IpcClient::connect(&path).await.unwrap();

        let id1 = client.send("method1", json!(null)).await.unwrap();
        let id2 = client.send("method2", json!(null)).await.unwrap();
        let id3 = client.send("method3", json!(null)).await.unwrap();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);

        drop(client);
        let _ = server_handle.await;
    }
}
