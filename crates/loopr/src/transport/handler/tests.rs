#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::{Notify, broadcast};

use ipc::{DaemonRequest, HandshakeParams, HandshakeResult, PROTOCOL_VERSION, RpcError, StatusResult};
use telemetry::RunId;

use super::*;
use crate::daemon::DaemonContext;

fn stub_ctx() -> Arc<DaemonContext> {
    let (events, _) = broadcast::channel(16);
    Arc::new(DaemonContext {
        target: PathBuf::from("/tmp"),
        run_id: RunId::parse("20260419-000000").unwrap(),
        started_at: chrono::Local::now(),
        pid: 12345,
        events,
        shutting_down: Arc::new(AtomicBool::new(false)),
        shutdown_notify: Arc::new(Notify::new()),
    })
}

fn handshake_req(id: u64, version: u32) -> DaemonRequest {
    DaemonRequest {
        id,
        method: "system.handshake".into(),
        params: serde_json::to_value(HandshakeParams {
            protocol_version: version,
        })
        .unwrap(),
    }
}

fn status_req(id: u64) -> DaemonRequest {
    DaemonRequest {
        id,
        method: "system.status".into(),
        params: serde_json::Value::Null,
    }
}

#[tokio::test]
async fn handshake_completes_state() {
    let ctx = stub_ctx();
    let mut state = HandshakeState::Pending;
    let resp = dispatch(&handshake_req(1, PROTOCOL_VERSION), &mut state, &ctx).await;
    assert_eq!(resp.id, 1);
    assert!(resp.error.is_none(), "no error: {resp:?}");
    let result: HandshakeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.protocol_version, PROTOCOL_VERSION);
    assert_eq!(state, HandshakeState::Complete);
}

#[tokio::test]
async fn handshake_mismatch_returns_protocol_version_mismatch() {
    let ctx = stub_ctx();
    let mut state = HandshakeState::Pending;
    let resp = dispatch(&handshake_req(2, 99), &mut state, &ctx).await;
    assert_eq!(resp.id, 2);
    match resp.error {
        Some(RpcError::ProtocolVersionMismatch(msg)) => {
            assert!(msg.contains("client=99"), "msg: {msg}");
        }
        other => panic!("expected ProtocolVersionMismatch, got {other:?}"),
    }
    // State stays Pending; a subsequent correct handshake is allowed.
    assert_eq!(state, HandshakeState::Pending);
}

#[tokio::test]
async fn status_before_handshake_is_invalid_request() {
    let ctx = stub_ctx();
    let mut state = HandshakeState::Pending;
    let resp = dispatch(&status_req(3), &mut state, &ctx).await;
    assert_eq!(resp.id, 3);
    match resp.error {
        Some(RpcError::InvalidRequest(msg)) => {
            assert!(msg.contains("handshake required"), "msg: {msg}");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn status_after_handshake_returns_status_result() {
    let ctx = stub_ctx();
    let mut state = HandshakeState::Pending;
    dispatch(&handshake_req(4, PROTOCOL_VERSION), &mut state, &ctx).await;
    let resp = dispatch(&status_req(5), &mut state, &ctx).await;
    assert_eq!(resp.id, 5);
    assert!(resp.error.is_none());
    let result: StatusResult = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(result.pid, ctx.pid);
    assert_eq!(result.active_plans, 0);
    assert_eq!(result.active_works, 0);
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let ctx = stub_ctx();
    let mut state = HandshakeState::Complete;
    let req = DaemonRequest {
        id: 6,
        method: "plan.create".into(),
        params: serde_json::json!({"goal": "x"}),
    };
    let resp = dispatch(&req, &mut state, &ctx).await;
    assert_eq!(resp.id, 6);
    match resp.error {
        Some(RpcError::MethodNotFound(m)) => assert_eq!(m, "plan.create"),
        other => panic!("expected MethodNotFound, got {other:?}"),
    }
}

// RFC-8259 newline-safety: a literal `\n` inside a string field must not
// split the NDJSON line prematurely. `serde_json::to_string` ASCII-
// escapes `\n` to `\\n` in compact mode, so the only `0x0A` on the wire
// is the terminator appended by the framing layer.
#[test]
fn serde_json_compact_escapes_embedded_newlines() {
    let obj = serde_json::json!({"message": "line1\nline2"});
    let compact = serde_json::to_string(&obj).unwrap();
    // Wire form contains escaped `\\n`, NOT a bare 0x0A.
    assert!(compact.contains("\\n"), "compact: {compact}");
    assert!(!compact.contains('\n'), "no bare 0x0A on wire: {compact}");
    // Round-trip.
    let back: serde_json::Value = serde_json::from_str(&compact).unwrap();
    assert_eq!(back, obj);
}
