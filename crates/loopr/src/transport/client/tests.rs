#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard, Notify, broadcast};
use tokio::time::timeout;

use ipc::{PROTOCOL_VERSION, RpcError, StatusResult};
use telemetry::RunId;

use super::*;
use crate::daemon::DaemonContext;
use crate::transport::server::{accept_loop, bind_listener};

/// Serialize every test that reads/writes process-global env vars so the
/// `LOOPR_PROTOCOL_VERSION_OVERRIDE` mismatch test cannot leak into
/// parallel tests that depend on the normal version.
///
/// Uses `tokio::sync::Mutex` (not `std::sync::Mutex`) so the guard is safe
/// to hold across `.await` points inside the `#[tokio::test]` body.
async fn env_mutex() -> MutexGuard<'static, ()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(())).lock().await
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_handshake_then_status() {
    let _env_guard = env_mutex().await;
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf());
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let mut client = IpcClient::connect(&socket).await.unwrap();
    let hs = client.handshake().await.unwrap();
    assert_eq!(hs.protocol_version, PROTOCOL_VERSION);

    let (resp, events) = client
        .request(ipc::MethodName::SystemStatus, serde_json::Value::Null)
        .await
        .unwrap();
    assert!(resp.error.is_none());
    assert!(events.is_empty(), "stage 4 never emits events");
    let st: StatusResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(st.pid, ctx.pid);

    drop(client);
    ctx.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_request_raw_unknown_method_returns_method_not_found() {
    let _env_guard = env_mutex().await;
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf());
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let mut client = IpcClient::connect(&socket).await.unwrap();
    client.handshake().await.unwrap();
    let (resp, _) = client
        .request_raw("plan.create", serde_json::json!({"goal": "x"}))
        .await
        .unwrap();
    match resp.error {
        Some(RpcError::MethodNotFound(m)) => assert_eq!(m, "plan.create"),
        other => panic!("expected MethodNotFound, got {other:?}"),
    }

    drop(client);
    ctx.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_handshake_version_mismatch_closes_connection() {
    let _env_guard = env_mutex().await;
    // Use the debug-build-only override env var to send a bogus
    // protocol_version. The daemon rejects with ProtocolVersionMismatch
    // and closes the connection.
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf());
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    // SAFETY: we set + unset the env var inside this single test; no
    // parallel test touches LOOPR_PROTOCOL_VERSION_OVERRIDE.
    unsafe {
        std::env::set_var("LOOPR_PROTOCOL_VERSION_OVERRIDE", "9999");
    }
    let mut client = IpcClient::connect(&socket).await.unwrap();
    let err = client.handshake().await.unwrap_err();
    unsafe {
        std::env::remove_var("LOOPR_PROTOCOL_VERSION_OVERRIDE");
    }
    match err {
        crate::error::LooprError::HandshakeFailed(msg) => {
            assert!(msg.contains("protocol version mismatch"), "msg: {msg}");
        }
        other => panic!("expected HandshakeFailed, got {other:?}"),
    }

    drop(client);
    ctx.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}
