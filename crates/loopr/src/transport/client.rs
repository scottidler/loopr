//! Client-side IPC transport: connect to `.loopr/socket`, handshake,
//! one-shot request/response.
//!
//! Short-lived by design: every CLI invocation builds a fresh
//! `IpcClient`, runs its handshake, issues one request, reads the
//! response (plus any events that arrived during the wait), and drops.
//! Long-lived bidirectional streams (TUI, `loopr events tail`) are a
//! future stage's concern.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

use ipc::{
    DaemonEvent, DaemonRequest, DaemonResponse, HandshakeParams, HandshakeResult, IpcMessage, MethodName,
    PROTOCOL_VERSION, decode_line,
};

use crate::error::LooprError;

/// Short-lived IPC client. Owns a `Framed` sink + stream over the Unix
/// socket; drops on exit.
pub struct IpcClient {
    framed: Framed<UnixStream, LinesCodec>,
    next_id: AtomicU64,
}

impl IpcClient {
    /// Connect to the daemon's socket. Returns `ClientIo` on any
    /// failure; callers that expect "daemon might still be starting up"
    /// should use `transport::connect_or_wait` instead.
    pub async fn connect(socket: &Path) -> Result<Self, LooprError> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| LooprError::ClientIo(format!("connect {}: {e}", socket.display())))?;
        let framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
        Ok(Self {
            framed,
            next_id: AtomicU64::new(1),
        })
    }

    /// Run the mandatory `system.handshake`. Must be the first request
    /// on every connection per the Stage 3 contract.
    pub async fn handshake(&mut self) -> Result<HandshakeResult, LooprError> {
        let params = HandshakeParams {
            protocol_version: protocol_version_or_override(),
        };
        let params_json =
            serde_json::to_value(params).map_err(|e| LooprError::HandshakeFailed(format!("serialize params: {e}")))?;
        let (resp, _events) = self.request_impl("system.handshake", params_json).await?;
        if let Some(err) = resp.error {
            return Err(LooprError::HandshakeFailed(err.to_string()));
        }
        let result_value = resp
            .result
            .ok_or_else(|| LooprError::HandshakeFailed("no result in handshake response".into()))?;
        serde_json::from_value(result_value).map_err(|e| LooprError::HandshakeFailed(format!("bad result: {e}")))
    }

    /// Typed request: common path. `MethodName` round-trips to the wire
    /// via `strum::Display`; adding a variant to `MethodName` in a
    /// future stage picks up here for free.
    pub async fn request(
        &mut self,
        method: MethodName,
        params: serde_json::Value,
    ) -> Result<(DaemonResponse, Vec<DaemonEvent>), LooprError> {
        let method_str: &'static str = method.into();
        self.request_impl(method_str, params).await
    }

    /// Untyped request: Stage-4-only escape hatch for methods that have
    /// not yet been promoted into `MethodName`. Used by `loopr plan "x"`
    /// to send "plan.create" before Stage 5 adds the variant; the daemon
    /// returns `RpcError::MethodNotFound` which the caller maps to
    /// `LooprError::StageUnimplemented`.
    pub async fn request_raw(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(DaemonResponse, Vec<DaemonEvent>), LooprError> {
        self.request_impl(method, params).await
    }

    async fn request_impl(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(DaemonResponse, Vec<DaemonEvent>), LooprError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = DaemonRequest {
            id,
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&req).map_err(|e| LooprError::ClientIo(format!("serialize request: {e}")))?;
        self.framed
            .send(line)
            .await
            .map_err(|e| LooprError::ClientIo(format!("send: {e}")))?;

        // Collect messages until we get the response whose id matches.
        // Broadcast events from the daemon interleave with responses; we
        // buffer them for the caller (Stage 4's callers ignore them).
        let mut events: Vec<DaemonEvent> = Vec::new();
        loop {
            let line = match self.framed.next().await {
                Some(Ok(line)) => line,
                Some(Err(e)) => return Err(LooprError::ClientIo(format!("codec error: {e}"))),
                None => return Err(LooprError::ClientIo("daemon closed connection".into())),
            };
            let msg = decode_line(line.as_bytes()).map_err(|e| LooprError::ClientIo(format!("decode: {e}")))?;
            match msg {
                IpcMessage::Response(resp) if resp.id == id => return Ok((resp, events)),
                IpcMessage::Response(other) => {
                    // Stage 4's connection-scoped request/response model
                    // guarantees ids are monotonic within a connection;
                    // a mismatch means the daemon replied out of order
                    // or re-used an id, which is a protocol error.
                    return Err(LooprError::ClientIo(format!(
                        "response id mismatch: expected {id}, got {}",
                        other.id
                    )));
                }
                IpcMessage::Event(ev) => events.push(ev),
            }
        }
    }
}

/// Protocol-version advertisement with a test-only override hook.
///
/// Release builds ALWAYS return `ipc::PROTOCOL_VERSION`. Debug builds
/// (and `#[cfg(test)]`) read `LOOPR_PROTOCOL_VERSION_OVERRIDE` first, so
/// the acceptance test for version-mismatch rejection can exercise the
/// unhappy path without a second binary build.
#[cfg(test)]
mod tests;

pub fn protocol_version_or_override() -> u32 {
    if cfg!(debug_assertions)
        && let Ok(v) = std::env::var("LOOPR_PROTOCOL_VERSION_OVERRIDE")
        && let Ok(n) = v.parse::<u32>()
    {
        return n;
    }
    PROTOCOL_VERSION
}
