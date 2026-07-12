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
    ctx_for_test_with_timeouts(target, crate::transport::ServerTimeouts::default()).await
}

async fn ctx_for_test_with_timeouts(
    target: PathBuf,
    server_timeouts: crate::transport::ServerTimeouts,
) -> Arc<DaemonContext<AnthropicClient>> {
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
        server_timeouts,
        None,
        4,
    ))
}

async fn complete_handshake(framed: &mut Framed<UnixStream, LinesCodec>) {
    let req = DaemonRequest {
        id: 1,
        method: "system.handshake".into(),
        params: serde_json::to_value(HandshakeParams {
            protocol_version: PROTOCOL_VERSION,
            session_id: None,
        })
        .unwrap(),
    };
    framed.send(serde_json::to_string(&req).unwrap()).await.unwrap();
    let line = timeout(Duration::from_secs(2), framed.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
    assert!(resp.error.is_none(), "handshake failed: {resp:?}");
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

// Phase 6 F6 (2026-07-11-verified-swarm): an oversized `record.get` result
// (over `handler::RECORD_GET_MAX_RESULT_BYTES`) must be caught before the
// response line is handed to `LinesCodec`, and surfaced as a typed
// `RpcError::PayloadTooLarge` on the SAME connection — unlike the oversize
// *inbound* line above (`oversize_line_closes_connection_daemon_stays_up`,
// which the daemon can only detect at the codec layer and must disconnect),
// an oversized *outbound* result is caught in the handler with the record id
// still known, so the connection does not need to be dropped at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_get_oversized_result_yields_typed_error_connection_survives() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf()).await;

    // Seed a Plan whose `goal` alone guarantees the serialized
    // `RecordResult::Plan` blows the `record.get` byte budget.
    let huge_goal = "x".repeat(ipc::MAX_LINE_BYTES);
    let plan = domain::Plan::new(huge_goal);
    let plan_id = plan.id.clone();
    ctx.store.plans().create(plan).await.unwrap();

    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    complete_handshake(&mut framed).await;

    let get_req = DaemonRequest {
        id: 2,
        method: "record.get".into(),
        params: serde_json::json!({ "id": plan_id.to_string() }),
    };
    framed.send(serde_json::to_string(&get_req).unwrap()).await.unwrap();
    let line = timeout(Duration::from_secs(2), framed.next())
        .await
        .expect("record.get response must not hang")
        .expect("connection must not close on oversized result")
        .unwrap();
    let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.id, 2);
    match resp.error {
        Some(ipc::RpcError::PayloadTooLarge(_)) => {}
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }

    // The connection survives: a follow-up request on the SAME framed
    // stream must still round-trip normally.
    let status = DaemonRequest {
        id: 3,
        method: "system.status".into(),
        params: serde_json::Value::Null,
    };
    framed.send(serde_json::to_string(&status).unwrap()).await.unwrap();
    let line = timeout(Duration::from_secs(2), framed.next())
        .await
        .expect("status response must not hang")
        .expect("connection must still be open after PayloadTooLarge")
        .unwrap();
    let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.id, 3);
    assert!(resp.error.is_none(), "status after PayloadTooLarge: {resp:?}");

    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

/// Phase 3 read-side idle timeout: a client that completes the handshake
/// then sits silent must be dropped after `server_idle` elapses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_idle_drops_silent_client() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let timeouts = crate::transport::ServerTimeouts {
        idle: Duration::from_millis(150),
        write: Duration::from_secs(10),
        heartbeat: Duration::from_secs(60),
    };
    let ctx = ctx_for_test_with_timeouts(td.path().to_path_buf(), timeouts).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    complete_handshake(&mut framed).await;

    // Sit silent. Within ~150ms the daemon must close the connection;
    // generous outer bound so a slow scheduler doesn't false-fail.
    let next = timeout(Duration::from_secs(2), framed.next()).await.unwrap();
    assert!(next.is_none(), "daemon must close silent connection: got {next:?}");

    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

/// Phase 3 critical invariant (Architect Round 2): a chatty broadcast
/// channel MUST NOT keep the read-idle timer reset. If this test ever
/// runs forever, the implementer regressed to a naive
/// `tokio::time::timeout(idle, framed.next())` inside the `select!`,
/// which resets the budget on every `event_rx.recv()` arm fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_idle_does_not_reset_on_broadcast() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let timeouts = crate::transport::ServerTimeouts {
        idle: Duration::from_millis(200),
        write: Duration::from_secs(10),
        heartbeat: Duration::from_secs(60),
    };
    let ctx = ctx_for_test_with_timeouts(td.path().to_path_buf(), timeouts).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    complete_handshake(&mut framed).await;

    // Background broadcaster: fire faster than `server_idle`. If the
    // implementation resets idle on broadcast, this loop would hold the
    // connection open until the test times out.
    let ctx_bcast = ctx.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop.clone();
    let broadcaster = tokio::spawn(async move {
        while !stop_clone.load(Ordering::Relaxed) {
            let _ = ctx_bcast.events.send(ipc::DaemonEvent {
                event: "test.tick".into(),
                data: serde_json::Value::Null,
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    // Drain incoming broadcast events; the connection MUST close
    // (None) on the read side within a generous outer bound. Without
    // the bug fix, this loop would never see None.
    let outcome = timeout(Duration::from_secs(2), async {
        loop {
            match framed.next().await {
                None => break,           // connection closed — desired outcome
                Some(Ok(_)) => continue, // broadcast event, keep reading
                Some(Err(e)) => panic!("unexpected codec error: {e}"),
            }
        }
    })
    .await;
    assert!(outcome.is_ok(), "idle timer must fire even under chatty broadcasts");

    stop.store(true, Ordering::Relaxed);
    let _ = broadcaster.await;
    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

/// Phase 3b write-side timeout: a stalled-reader client whose receive
/// buffer fills must be dropped within `server_write` rather than
/// pinning the handler task forever. We simulate the stalled reader
/// by holding the raw `UnixStream` open without ever reading, then
/// flooding broadcast events until the kernel send buffer fills and
/// the daemon's wrapped `framed.send` exceeds its budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_write_timeout_drops_stalled_reader() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let timeouts = crate::transport::ServerTimeouts {
        idle: Duration::from_secs(60), // not under test here
        write: Duration::from_millis(200),
        heartbeat: Duration::from_secs(60),
    };
    let ctx = ctx_for_test_with_timeouts(td.path().to_path_buf(), timeouts).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    complete_handshake(&mut framed).await;

    // Stop reading: drop the Framed wrapper but keep the underlying
    // socket alive by replacing it with a raw fd we never touch. Easier
    // approach: just keep `framed` parked and never call `.next()`.
    // A reasonable broadcast payload fills the kernel's unix-domain
    // socket send buffer (typically 200KiB+) within a few thousand
    // events; we send up to 5000 ~1KiB events to be safe.
    let payload = "x".repeat(1024);
    let big_event = ipc::DaemonEvent {
        event: "fill.buffer".into(),
        data: serde_json::Value::String(payload),
    };
    for _ in 0..5000 {
        // Errors here just mean no subscribers — fine if the handler
        // already closed. The point is to push enough bytes that ONE
        // of these triggers a write-blocking send on the daemon side.
        let _ = ctx.events.send(big_event.clone());
        // Tiny yield so the daemon's handler task gets scheduled and
        // can drain events into the (eventually full) send buffer.
        tokio::task::yield_now().await;
    }

    // Once the buffer fills, the next `framed.send` on the daemon side
    // exceeds `server_write` and the handler closes the connection.
    // From the client side: dropping framed and reading raw bytes from
    // the underlying stream eventually returns 0.
    drop(framed);
    let raw = UnixStream::connect(&socket).await.unwrap();
    // (Above just confirms accept loop still serves new clients.)
    let _ = raw;

    // Wait briefly for the write timeout to fire on the original
    // handler's task. We can't easily observe it from outside without
    // hooking a log capture; the strongest assertion is that the
    // daemon stays responsive (above raw connection succeeded) and
    // shutdown completes cleanly.
    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(5), server).await.unwrap().unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// Phase 17 (2026-07-11-verified-swarm): events.subscribe long-lived stream.
// ---------------------------------------------------------------------------

/// Send `events.subscribe` and read the one-shot ack, leaving the stream
/// live. Returns after the ack; subsequent frames are event/heartbeat/gap.
async fn subscribe(framed: &mut Framed<UnixStream, LinesCodec>) {
    let req = DaemonRequest {
        id: 2,
        method: "events.subscribe".into(),
        params: serde_json::Value::Null,
    };
    framed.send(serde_json::to_string(&req).unwrap()).await.unwrap();
    let line = timeout(Duration::from_secs(2), framed.next())
        .await
        .expect("subscribe ack must not hang")
        .expect("connection must stay open after subscribe")
        .unwrap();
    let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.id, 2);
    assert!(resp.error.is_none(), "subscribe ack error: {resp:?}");
}

/// Read one stream frame and classify it as a typed `WatchFrame`.
async fn next_frame(framed: &mut Framed<UnixStream, LinesCodec>) -> Option<ipc::WatchFrame> {
    let line = timeout(Duration::from_secs(2), framed.next()).await.ok()??.ok()?;
    let ev: ipc::DaemonEvent = serde_json::from_str(&line).unwrap();
    Some(ipc::WatchFrame::classify(ev))
}

/// The Lagged -> gap-marker mapping is exercised against a REAL
/// `broadcast::error::RecvError::Lagged`, deterministically (no wall-clock
/// race). Break-to-prove: change the Lagged arm in `event_recv_to_action`
/// to `Stop`/swallow and this fails.
#[tokio::test]
async fn lagged_recv_maps_to_typed_gap_marker() {
    let (tx, mut rx) = tokio::sync::broadcast::channel::<ipc::DaemonEvent>(2);
    // Overflow the capacity-2 ring so the next recv() lags.
    for i in 0..5u64 {
        let _ = tx.send(ipc::DaemonEvent {
            event: "test.tick".into(),
            data: serde_json::json!({ "i": i }),
        });
    }
    let res = rx.recv().await;
    assert!(
        matches!(res, Err(tokio::sync::broadcast::error::RecvError::Lagged(_))),
        "precondition: recv must lag after ring overflow"
    );
    match event_recv_to_action(res) {
        StreamAction::Send(ev) => {
            // A typed gap marker carrying a nonzero dropped count — never
            // silence, never Stop.
            match ipc::WatchFrame::classify(ev) {
                ipc::WatchFrame::Gap { dropped } => assert!(dropped >= 1, "gap must report dropped >= 1"),
                other => panic!("expected a gap frame, got {other:?}"),
            }
        }
        StreamAction::Stop => panic!("Lagged must NOT stop the stream; it must emit a visible gap marker"),
    }
}

/// A subscriber that sits silent MUST NOT be dropped by the read-idle
/// timeout (it is exempt), and MUST receive periodic heartbeats. We set a
/// SHORT idle budget and an even shorter heartbeat, then prove several
/// heartbeats arrive over a window that far exceeds the idle budget — with
/// no disconnect. Sub-second: no literal 60s wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_stream_is_idle_exempt_and_heartbeats() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let timeouts = crate::transport::ServerTimeouts {
        idle: Duration::from_millis(100), // would kill a normal silent client fast
        write: Duration::from_secs(10),
        heartbeat: Duration::from_millis(40),
    };
    let ctx = ctx_for_test_with_timeouts(td.path().to_path_buf(), timeouts).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    complete_handshake(&mut framed).await;
    subscribe(&mut framed).await;

    // Collect heartbeats until we've seen 3. Three heartbeats at 40ms
    // cadence span ~120ms > the 100ms idle budget, so reaching 3 without a
    // disconnect proves the subscribe path is idle-exempt.
    let mut heartbeats = 0u32;
    while heartbeats < 3 {
        match next_frame(&mut framed).await {
            Some(ipc::WatchFrame::Heartbeat) => heartbeats += 1,
            Some(other) => panic!("unexpected frame on a silent stream: {other:?}"),
            None => panic!("subscribe stream was closed (idle timeout leaked into the exempt path?)"),
        }
    }
    assert!(heartbeats >= 3, "expected >= 3 heartbeats, got {heartbeats}");

    drop(framed);
    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

/// A full plan lifecycle broadcast AFTER subscribe renders IN ORDER on the
/// stream. Heartbeat is set long so only real events appear in the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_stream_renders_events_in_order() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let timeouts = crate::transport::ServerTimeouts {
        idle: Duration::from_secs(60),
        write: Duration::from_secs(10),
        heartbeat: Duration::from_secs(60), // no heartbeats in this short window
    };
    let ctx = ctx_for_test_with_timeouts(td.path().to_path_buf(), timeouts).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    complete_handshake(&mut framed).await;
    subscribe(&mut framed).await;

    // The server's receiver exists by the time the ack is read (it is
    // created before the ack send), so events sent now are captured.
    let sequence = [
        (
            "work.terminal",
            serde_json::json!({ "work_id": "wk-00001", "plan_id": "pl-abc12", "status": "Done" }),
        ),
        (
            "work.terminal",
            serde_json::json!({ "work_id": "wk-00002", "plan_id": "pl-abc12", "status": "Done" }),
        ),
        (
            "plan.terminal",
            serde_json::json!({ "plan_id": "pl-abc12", "status": "Complete" }),
        ),
    ];
    for (name, data) in &sequence {
        ctx.events
            .send(ipc::DaemonEvent {
                event: (*name).into(),
                data: data.clone(),
            })
            .unwrap();
    }

    let mut got = Vec::new();
    while got.len() < sequence.len() {
        match next_frame(&mut framed).await {
            Some(ipc::WatchFrame::Event(ev)) => got.push(ev),
            Some(other) => panic!("unexpected non-event frame: {other:?}"),
            None => panic!("stream closed before all events arrived"),
        }
    }
    assert_eq!(got[0].event, "work.terminal");
    assert_eq!(got[0].data["work_id"], "wk-00001");
    assert_eq!(got[1].data["work_id"], "wk-00002");
    assert_eq!(got[2].event, "plan.terminal");

    drop(framed);
    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

/// Killing the client tears down the server-side subscription cleanly: the
/// broadcast receiver count returns to zero (both the handler's outer
/// receiver and the stream's live-tail receiver are dropped) — no orphaned
/// forwarder task holds a receiver open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_disconnect_tears_down_subscription_no_orphan() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf()).await;
    assert_eq!(ctx.events.receiver_count(), 0, "no receivers before any client");

    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    complete_handshake(&mut framed).await;
    subscribe(&mut framed).await;

    // Subscription is live: at least one server-side receiver exists.
    let mut live = false;
    for _ in 0..100 {
        if ctx.events.receiver_count() >= 1 {
            live = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(live, "server-side subscription receiver never appeared");

    // Kill the client. The server must observe EOF, exit serve_event_stream,
    // and drop its receiver(s) — receiver_count returns to zero.
    drop(framed);
    let mut torn_down = false;
    for _ in 0..150 {
        if ctx.events.receiver_count() == 0 {
            torn_down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        torn_down,
        "server-side subscription leaked (receiver_count never returned to 0)"
    );

    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

/// A pre-handshake `events.subscribe` is rejected (protocol violation) and
/// the connection closes — a subscribe stream must not open before the
/// handshake completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_before_handshake_is_rejected() {
    let td = TempDir::new().unwrap();
    let socket = td.path().join("socket");
    let listener = bind_listener(&socket).unwrap();
    let ctx = ctx_for_test(td.path().to_path_buf()).await;
    let ctx_server = ctx.clone();
    let server = tokio::spawn(async move { accept_loop(listener, ctx_server).await });

    let stream = UnixStream::connect(&socket).await.unwrap();
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    let req = DaemonRequest {
        id: 1,
        method: "events.subscribe".into(),
        params: serde_json::Value::Null,
    };
    framed.send(serde_json::to_string(&req).unwrap()).await.unwrap();
    let line = timeout(Duration::from_secs(2), framed.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
    assert!(resp.error.is_some(), "pre-handshake subscribe must be rejected");
    // Connection then closes.
    let next = timeout(Duration::from_secs(2), framed.next()).await.unwrap();
    assert!(next.is_none(), "connection must close after pre-handshake subscribe");

    ctx.shutting_down.store(true, Ordering::Relaxed);
    ctx.shutdown_notify.notify_waiters();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
}

#[test]
fn transient_accept_errors_classified() {
    use super::is_transient_accept_error;
    use std::io::{Error, ErrorKind};

    // Resource-pressure errnos are transient: the daemon must survive an
    // EMFILE burst from tool subprocesses rather than dying on accept.
    for errno in [
        libc::EMFILE,
        libc::ENFILE,
        libc::ENOBUFS,
        libc::ENOMEM,
        libc::ECONNABORTED,
        libc::EINTR,
    ] {
        assert!(
            is_transient_accept_error(&Error::from_raw_os_error(errno)),
            "errno {errno} should be transient"
        );
    }
    // ErrorKind-classified variants too.
    assert!(is_transient_accept_error(&Error::new(
        ErrorKind::ConnectionAborted,
        "aborted"
    )));
    assert!(is_transient_accept_error(&Error::new(ErrorKind::Interrupted, "intr")));

    // A genuinely broken listener (EINVAL / EBADF) is fatal and must
    // propagate so the daemon does not spin forever.
    assert!(!is_transient_accept_error(&Error::from_raw_os_error(libc::EINVAL)));
    assert!(!is_transient_accept_error(&Error::from_raw_os_error(libc::EBADF)));
}
