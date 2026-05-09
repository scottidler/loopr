#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::codec::{Framed, LinesCodec};

use agents::{DirectorConfig, ImplementerConfig, ReviewerConfig};
use context::InlineContextBuilder;
use ipc::{DaemonRequest, DaemonResponse, HandshakeParams, PROTOCOL_VERSION};
use llm::{AnthropicClient, LlmConfig};
use store::Store;
use telemetry::{ProcessId, SessionId};
use tempfile::TempDir;
use tools::{BashDenylist, LaneRouter, SandboxMode};
use worktree::AttemptCleanupPolicy;

use super::*;

fn dummy_llm() -> Arc<AnthropicClient> {
    let cfg = LlmConfig {
        api_base_url: "http://127.0.0.1:1".to_string(),
        ..LlmConfig::default()
    };
    Arc::new(AnthropicClient::new(cfg, "test-key".to_string()).unwrap())
}

async fn ctx_for_test(target: PathBuf) -> Arc<DaemonContext<AnthropicClient>> {
    let store = Store::open(&target).await.unwrap();
    let router = Arc::new(LaneRouter::new(SandboxMode::Off).unwrap());
    let bash_denylist = Arc::new(BashDenylist::with_base());
    let snapshot = Arc::new(std::sync::Mutex::new(telemetry::digest::process::ProcessSnapshot::new(
        "test-stub-model",
    )));
    Arc::new(DaemonContext::new(
        target,
        SessionId::parse("20260419-000000").unwrap(),
        "-test-target".to_string(),
        ProcessId::parse("pc-test01").unwrap(),
        std::process::id(),
        store,
        dummy_llm(),
        router,
        bash_denylist,
        Vec::new(),
        SandboxMode::Off,
        Arc::new(InlineContextBuilder::new()),
        ImplementerConfig::default(),
        ReviewerConfig::default(),
        integrator::IntegratorConfig::default(),
        DirectorConfig::default(),
        AttemptCleanupPolicy::default(),
        snapshot,
    ))
}

/// Spawn an accept loop, connect as a client, drive a handshake +
/// status request/response round-trip, then shut the loop down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_then_status_roundtrip() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf()).await;
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
            session_id: None,
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
    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_line_closes_connection() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf()).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    framed.send("not json".to_string()).await.unwrap();
    // Daemon closes the connection on parse error; `framed.next()` sees
    // EOF (None).
    let next = timeout(Duration::from_secs(2), framed.next()).await.unwrap();
    assert!(next.is_none(), "connection closed after parse error: {next:?}");

    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_clients_do_not_cross_correlate() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf()).await;
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
                session_id: None,
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

    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

// AC 20: a 2-MiB line (>1 MiB MAX_LINE_BYTES) must close the offending
// connection without tearing the daemon down. We bypass `Framed` on the
// client side and write raw bytes via `AsyncWriteExt` so LinesCodec's
// encoder doesn't refuse the oversize frame before it leaves the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_line_closes_connection_daemon_stays_up() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf()).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    // First connection: send 2 MiB + newline, expect disconnect.
    //
    // The daemon closes the connection as soon as LinesCodec's internal
    // buffer crosses MAX_LINE_BYTES, which can happen mid-write. The
    // kernel may then return EPIPE on any subsequent `write_all`. We
    // tolerate that: either the full payload goes out and the read
    // returns 0, or the write itself errors because the peer closed.
    {
        let mut raw = UnixStream::connect(&socket).await.unwrap();
        let payload = vec![b'x'; 2 * 1024 * 1024];
        let _ = raw.write_all(&payload).await;
        let _ = raw.write_all(b"\n").await;
        let _ = raw.flush().await;
        let mut buf = [0u8; 64];
        let read = timeout(
            Duration::from_secs(3),
            tokio::io::AsyncReadExt::read(&mut raw, &mut buf),
        )
        .await
        .unwrap();
        match read {
            Ok(0) => {} // clean close after LineTooLong
            Ok(n) => panic!("unexpected payload back from daemon: {n} bytes"),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            // ECONNRESET: server closed mid-stream with data still in flight.
            // Same root cause as UnexpectedEof; depends on kernel RST timing.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(e) => panic!("unexpected read error: {e}"),
        }
    }

    // Second connection: a well-formed client can still handshake +
    // status, proving the daemon itself is still up.
    {
        let stream = UnixStream::connect(&socket).await.unwrap();
        let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
        let handshake = DaemonRequest {
            id: 1,
            method: "system.handshake".into(),
            params: serde_json::to_value(HandshakeParams {
                protocol_version: PROTOCOL_VERSION,
                session_id: None,
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
        assert!(resp.error.is_none(), "daemon survived oversize client: {resp:?}");
    }

    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}
