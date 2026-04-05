use std::path::{Path, PathBuf};

use futures::SinkExt;
use futures::future::BoxFuture;
use log::debug;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_util::codec::Framed;

use super::codec::ndjson_codec;
use super::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

/// Unix socket IPC server for the daemon.
/// Accepts client connections, frames them with NDJSON, and dispatches
/// requests to a handler function. Events are broadcast to all connected clients
/// via an externally-owned broadcast channel.
pub struct IpcServer {
    socket_path: PathBuf,
}

impl IpcServer {
    /// Create a new IPC server bound to the given socket path.
    /// The event broadcast channel is owned externally (by DaemonContext).
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        let socket_path = socket_path.into();
        debug!("IpcServer::new(socket_path={})", socket_path.display());
        Self { socket_path }
    }

    /// Get the socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Bind the Unix socket listener.
    /// Removes any stale socket file before binding.
    pub async fn bind(&self) -> std::io::Result<UnixListener> {
        debug!("IpcServer::bind(socket_path={})", self.socket_path.display());
        // Remove stale socket if it exists
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        UnixListener::bind(&self.socket_path)
    }

    /// Cleanup the socket file on shutdown.
    pub fn cleanup(&self) {
        debug!("IpcServer::cleanup()");
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Handle a single client connection.
/// Reads NDJSON requests, dispatches to the handler, writes responses,
/// and forwards broadcast events to the client.
pub async fn handle_client(
    stream: UnixStream,
    handler: impl Fn(DaemonRequest) -> BoxFuture<'static, DaemonResponse> + Send + 'static,
    mut event_rx: broadcast::Receiver<DaemonEvent>,
) {
    debug!("handle_client()");
    let mut framed = Framed::new(stream, ndjson_codec());

    loop {
        tokio::select! {
            // Read a request from the client
            line = framed.next() => {
                match line {
                    Some(Ok(line)) => {
                        let response = match serde_json::from_str::<DaemonRequest>(&line) {
                            Ok(req) => handler(req).await,
                            Err(_) => {
                                // Can't parse → send error with id=0
                                DaemonResponse::err(0, RpcError::invalid_params("malformed request JSON"))
                            }
                        };
                        let resp_line = match serde_json::to_string(&response) {
                            Ok(s) => s,
                            Err(_) => break, // serialization failure is fatal
                        };
                        if framed.send(resp_line).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Some(Err(_)) => break,  // codec error
                    None => break,          // client disconnected
                }
            }
            // Forward broadcast events to this client
            event = event_rx.recv() => {
                match event {
                    Ok(event) => {
                        let event_line = match serde_json::to_string(&event) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        if framed.send(event_line).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    fn temp_socket_path() -> PathBuf {
        let dir = std::env::temp_dir().join("loopr-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("test-{}.sock", crate::id::generate_id("xx")))
    }

    #[tokio::test]
    async fn test_server_bind_and_cleanup() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);

        let listener = server.bind().await.unwrap();
        assert!(path.exists());

        // Accept is ready (no clients yet, so we just verify it bound)
        drop(listener);
        server.cleanup();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_server_removes_stale_socket() {
        let path = temp_socket_path();

        // Create a stale socket file
        std::fs::write(&path, "stale").unwrap();
        assert!(path.exists());

        let server = IpcServer::new(&path);
        let _listener = server.bind().await.unwrap();
        // Should have removed stale file and bound successfully
        assert!(path.exists());

        server.cleanup();
    }

    #[tokio::test]
    async fn test_socket_path_accessor() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);
        assert_eq!(server.socket_path(), path);
    }

    #[tokio::test]
    async fn test_handle_client_request_response() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();

        let (tx, _) = broadcast::channel::<DaemonEvent>(16);
        let event_rx = tx.subscribe();

        // Spawn the client handler
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_client(
                stream,
                |req| {
                    Box::pin(async move { DaemonResponse::ok(req.id, json!({"echo": req.method})) })
                        as BoxFuture<'static, DaemonResponse>
                },
                event_rx,
            )
            .await;
        });

        // Connect as client
        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Send a request
        let req = DaemonRequest::new(1, "system.handshake", json!({"version": "0.1.0"}));
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        writer.write_all(line.as_bytes()).await.unwrap();

        // Read the response
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).await.unwrap();
        let resp: DaemonResponse = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.id, 1);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["echo"], "system.handshake");

        // Close client → handler exits
        drop(writer);
        drop(reader);
        let _ = server_task.await;
        server.cleanup();
    }

    #[tokio::test]
    async fn test_handle_client_malformed_request() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = broadcast::channel::<DaemonEvent>(16);
        let event_rx = tx.subscribe();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_client(
                stream,
                |req| {
                    Box::pin(async move { DaemonResponse::ok(req.id, json!(null)) })
                        as BoxFuture<'static, DaemonResponse>
                },
                event_rx,
            )
            .await;
        });

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Send malformed JSON
        writer.write_all(b"not valid json\n").await.unwrap();

        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).await.unwrap();
        let resp: DaemonResponse = serde_json::from_str(&resp_line).unwrap();
        assert_eq!(resp.id, 0);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("malformed"));

        drop(writer);
        drop(reader);
        let _ = server_task.await;
        server.cleanup();
    }

    #[tokio::test]
    async fn test_handle_client_receives_broadcast_events() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = broadcast::channel::<DaemonEvent>(16);
        let event_rx = tx.subscribe();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_client(
                stream,
                |req| {
                    Box::pin(async move { DaemonResponse::ok(req.id, json!(null)) })
                        as BoxFuture<'static, DaemonResponse>
                },
                event_rx,
            )
            .await;
        });

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Small delay to ensure connection is established
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Broadcast an event
        let event = DaemonEvent::record_created("plan", "p1");
        tx.send(event.clone()).unwrap();

        // Client should receive the event
        let mut event_line = String::new();
        reader.read_line(&mut event_line).await.unwrap();
        let received: DaemonEvent = serde_json::from_str(&event_line).unwrap();
        assert_eq!(received.event, "record.created");
        assert_eq!(received.data["collection"], "plan");

        drop(writer);
        drop(reader);
        let _ = server_task.await;
        server.cleanup();
    }

    #[tokio::test]
    async fn test_handle_client_disconnect() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = broadcast::channel::<DaemonEvent>(16);
        let event_rx = tx.subscribe();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_client(
                stream,
                |req| {
                    Box::pin(async move { DaemonResponse::ok(req.id, json!(null)) })
                        as BoxFuture<'static, DaemonResponse>
                },
                event_rx,
            )
            .await;
            // handle_client should return cleanly when client disconnects
        });

        let stream = UnixStream::connect(&path).await.unwrap();
        // Immediately drop = disconnect
        drop(stream);

        // Server task should complete without error
        server_task.await.unwrap();
        drop(tx);
        server.cleanup();
    }

    #[tokio::test]
    async fn test_multiple_requests_on_same_connection() {
        let path = temp_socket_path();
        let server = IpcServer::new(&path);
        let listener = server.bind().await.unwrap();
        let (tx, _) = broadcast::channel::<DaemonEvent>(16);
        let event_rx = tx.subscribe();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_client(
                stream,
                |req| {
                    Box::pin(async move { DaemonResponse::ok(req.id, json!({"method": req.method})) })
                        as BoxFuture<'static, DaemonResponse>
                },
                event_rx,
            )
            .await;
        });

        let stream = UnixStream::connect(&path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Send 3 requests and verify 3 responses
        for i in 1..=3u64 {
            let req = DaemonRequest::new(i, format!("test.method_{i}"), json!(null));
            let mut line = serde_json::to_string(&req).unwrap();
            line.push('\n');
            writer.write_all(line.as_bytes()).await.unwrap();

            let mut resp_line = String::new();
            reader.read_line(&mut resp_line).await.unwrap();
            let resp: DaemonResponse = serde_json::from_str(&resp_line).unwrap();
            assert_eq!(resp.id, i);
            assert_eq!(resp.result.unwrap()["method"], format!("test.method_{i}"));
            resp_line.clear();
        }

        drop(writer);
        drop(reader);
        let _ = server_task.await;
        server.cleanup();
    }
}
