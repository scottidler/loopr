#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::{Notify, broadcast};
use tokio::time::timeout;
use tokio_util::codec::{Framed, LinesCodec};

use ipc::{DaemonRequest, DaemonResponse, HandshakeParams, PROTOCOL_VERSION};
use telemetry::RunId;
use tempfile::TempDir;

use super::*;

fn ctx_for_test(target: PathBuf) -> Arc<DaemonContext> {
    let (events, _) = broadcast::channel(16);
    Arc::new(DaemonContext {
        target,
        run_id: RunId::parse("20260419-000000").unwrap(),
        started_at: chrono::Local::now(),
        pid: std::process::id(),
        events,
        shutting_down: Arc::new(AtomicBool::new(false)),
        shutdown_notify: Arc::new(Notify::new()),
    })
}

/// Spawn an accept loop, connect as a client, drive a handshake +
/// status request/response round-trip, then shut the loop down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_then_status_roundtrip() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf());
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    // Client side.
    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));

    let handshake = DaemonRequest {
        id: 1,
        method: "system.handshake".into(),
        params: serde_json::to_value(HandshakeParams {
            protocol_version: PROTOCOL_VERSION,
        })
        .unwrap(),
    };
    framed.send(serde_json::to_string(&handshake).unwrap()).await.unwrap();
    let line = timeout(Duration::from_secs(2), framed.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.id, 1);
    assert!(resp.error.is_none(), "handshake no error: {resp:?}");

    let status = DaemonRequest {
        id: 2,
        method: "system.status".into(),
        params: serde_json::Value::Null,
    };
    framed.send(serde_json::to_string(&status).unwrap()).await.unwrap();
    let line = timeout(Duration::from_secs(2), framed.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.id, 2);
    assert!(resp.error.is_none(), "status no error: {resp:?}");

    // Drop the client to close its side, then shut the server down.
    drop(framed);
    ctx.shutting_down.store(true, Ordering::SeqCst);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_line_closes_connection() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf());
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    framed.send("not json".to_string()).await.unwrap();
    // Daemon closes the connection on parse error; `framed.next()` sees
    // EOF (None).
    let next = timeout(Duration::from_secs(2), framed.next()).await.unwrap();
    assert!(next.is_none(), "connection closed after parse error: {next:?}");

    ctx.shutting_down.store(true, Ordering::SeqCst);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_clients_do_not_cross_correlate() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf());
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    async fn one_client(socket: std::path::PathBuf, client_id: u64) -> (u64, u64) {
        let stream = UnixStream::connect(&socket).await.unwrap();
        let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
        let handshake = DaemonRequest {
            id: client_id * 10,
            method: "system.handshake".into(),
            params: serde_json::to_value(HandshakeParams {
                protocol_version: PROTOCOL_VERSION,
            })
            .unwrap(),
        };
        framed.send(serde_json::to_string(&handshake).unwrap()).await.unwrap();
        let line = framed.next().await.unwrap().unwrap();
        let hs_resp: DaemonResponse = serde_json::from_str(&line).unwrap();

        let status = DaemonRequest {
            id: client_id * 10 + 1,
            method: "system.status".into(),
            params: serde_json::Value::Null,
        };
        framed.send(serde_json::to_string(&status).unwrap()).await.unwrap();
        let line = framed.next().await.unwrap().unwrap();
        let st_resp: DaemonResponse = serde_json::from_str(&line).unwrap();
        (hs_resp.id, st_resp.id)
    }

    let mut tasks = Vec::new();
    for id in 1..=5 {
        let socket = socket.clone();
        tasks.push(tokio::spawn(async move { one_client(socket, id).await }));
    }
    for (i, t) in tasks.into_iter().enumerate() {
        let client_id = (i as u64) + 1;
        let (hs_id, st_id) = timeout(Duration::from_secs(3), t).await.unwrap().unwrap();
        assert_eq!(hs_id, client_id * 10, "handshake id preserved for client {client_id}");
        assert_eq!(st_id, client_id * 10 + 1, "status id preserved for client {client_id}");
    }

    ctx.shutting_down.store(true, Ordering::SeqCst);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}
