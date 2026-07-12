#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::timeout;

use agents::{DirectorConfig, ImplementerConfig, ReviewerConfig};
use context::InlineContextBuilder;
use ipc::{PROTOCOL_VERSION, RpcError, StatusResult};
use llm::{AnthropicClient, LlmConfig};
use store::Store;
use telemetry::{ProcessId, SessionId};
use tools::{BashDenylist, LaneRouter, SandboxMode};
use worktree::AttemptCleanupPolicy;

use super::*;
use crate::daemon::DaemonContext;
use crate::transport::server::{accept_loop, bind_listener};

fn dummy_llm() -> Arc<AnthropicClient> {
    let cfg = LlmConfig {
        api_base_url: "http://127.0.0.1:1".to_string(),
        ..LlmConfig::default()
    };
    Arc::new(AnthropicClient::new(cfg, "test-key".to_string()).unwrap())
}

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
        decomposer::DecomposerConfig::default(),
        AttemptCleanupPolicy::default(),
        snapshot,
        crate::transport::ServerTimeouts::default(),
        None,
        4,
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_handshake_then_status() {
    let _env_guard = env_mutex().await;
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf()).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let mut client = IpcClient::connect(&socket, crate::transport::ClientTimeouts::default())
        .await
        .unwrap();
    let hs = client.handshake(None).await.unwrap();
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
    let ctx = ctx_for_test(td.path().to_path_buf()).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let mut client = IpcClient::connect(&socket, crate::transport::ClientTimeouts::default())
        .await
        .unwrap();
    client.handshake(None).await.unwrap();
    let (resp, _) = client
        .request_raw("bogus.method", serde_json::json!({"goal": "x"}))
        .await
        .unwrap();
    match resp.error {
        Some(RpcError::MethodNotFound(m)) => assert_eq!(m, "bogus.method"),
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
    let ctx = ctx_for_test(td.path().to_path_buf()).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    // SAFETY: we set + unset the env var inside this single test; no
    // parallel test touches LOOPR_PROTOCOL_VERSION_OVERRIDE.
    unsafe {
        std::env::set_var("LOOPR_PROTOCOL_VERSION_OVERRIDE", "9999");
    }
    let mut client = IpcClient::connect(&socket, crate::transport::ClientTimeouts::default())
        .await
        .unwrap();
    let err = client.handshake(None).await.unwrap_err();
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

/// Phase 4 client request timeout: a daemon that accepts the connection
/// then never replies must surface as `ClientIo("request timed out
/// after Ns")` within roughly the configured budget. Built without the
/// real accept_loop so the daemon side simply receives bytes and
/// drops them; the client's wall-clock cap is what fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_request_times_out_against_silent_server() {
    let _env_guard = env_mutex().await;
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();

    // Sham server: accept the connection, then never reply. We don't run
    // the real `accept_loop` here because we want the daemon side to be
    // observably silent regardless of any handshake handling. Hold the
    // accepted stream until the test ends so the client's connection
    // doesn't see EOF.
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // Park the stream — never read, never write.
        tokio::time::sleep(Duration::from_secs(60)).await;
        drop(stream);
    });

    let timeouts = crate::transport::ClientTimeouts {
        request: Duration::from_millis(150),
        connect_wait: Duration::from_secs(3),
    };
    let mut client = IpcClient::connect(&socket, timeouts).await.unwrap();

    // The handshake itself routes through `request_impl`, so the
    // configured `request_budget` is what bounds the silent-server
    // hang. Anything calling `request_impl` against this server
    // should surface as `ClientIo("request timed out after ...")`.
    let start = std::time::Instant::now();
    let err = client.handshake(None).await.unwrap_err();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "timeout must fire promptly, took {elapsed:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("request timed out after"),
        "expected timeout error string, got: {msg}"
    );

    server.abort();
}

/// Phase 5 startup wait: a daemon running a crash-recovery reconcile binds
/// its socket only after `build_context` returns. That can exceed the
/// retired hard 3s client cap, which spuriously failed the connect. The
/// wait now defaults to the daemon startup budget (60s).
///
/// Break-to-prove is self-contained: the first connect uses a 3s
/// `connect_wait` (the old hard cap) and MUST give up before the slow bind;
/// the second uses the new default budget and MUST wait it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_connect_waits_out_slow_reconcile() {
    use crate::transport::connect_or_wait_with_timeouts;

    let td = TempDir::new().unwrap();
    let socket = td.path().join(".loopr").join("socket");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let socket_bg = socket.clone();

    // Slower than the retired 3s cap, well under the 60s default budget.
    const SLOW_BIND_MS: u64 = 4_500;
    let server = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(SLOW_BIND_MS)).await;
        let listener = bind_listener(&socket_bg).unwrap();
        // Hold the listener alive so the client's connect succeeds.
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(listener);
    });

    // Old hard 3s cap: gives up before the daemon binds.
    let short = crate::transport::ClientTimeouts {
        request: Duration::from_millis(200),
        connect_wait: Duration::from_secs(3),
    };
    let early = connect_or_wait_with_timeouts(td.path(), short).await;
    assert!(
        early.is_err(),
        "a 3s wait must give up before a {SLOW_BIND_MS}ms reconcile binds the socket"
    );

    // New default budget: waits out the slow reconcile and connects.
    // (`IpcClient` is not `Debug`, so assert on the discriminant, not `{:?}`.)
    let ok = connect_or_wait_with_timeouts(td.path(), crate::transport::ClientTimeouts::default()).await;
    assert!(
        ok.is_ok(),
        "default connect budget must outlast a slow reconcile (connect error: {:?})",
        ok.err()
    );

    server.abort();
}

/// Phase 5: while waiting for the socket, the client polls the daemon's
/// startup-error sentinel so a genuine boot failure surfaces immediately
/// with its real reason, instead of the client burning the full 60s budget
/// and reporting a generic "socket never appeared".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_connect_surfaces_startup_error_sentinel() {
    use crate::transport::connect_or_wait_with_timeouts;

    let td = TempDir::new().unwrap();
    std::fs::create_dir_all(td.path().join(".loopr")).unwrap();
    // Simulate the grandchild recording a fatal startup failure.
    crate::daemon::sentinel::write_startup_error(td.path(), "refusing to start: 2 corrupt record(s)");

    let start = std::time::Instant::now();
    // `IpcClient` is not `Debug`, so `unwrap_err()` won't compile; match instead.
    let err = match connect_or_wait_with_timeouts(td.path(), crate::transport::ClientTimeouts::default()).await {
        Ok(_) => panic!("connect must fail fast on a startup-error sentinel, not succeed"),
        Err(e) => e,
    };
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "sentinel must be observed promptly, not after the full 60s budget: took {elapsed:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("corrupt record"),
        "should carry the sentinel reason: {msg}"
    );
}
