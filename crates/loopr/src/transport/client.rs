//! Client-side IPC transport: connect to `.loopr/socket`, handshake,
//! one-shot request/response.
//!
//! Short-lived by design: every CLI invocation builds a fresh
//! `IpcClient`, runs its handshake, issues one request, reads the
//! response (plus any events that arrived during the wait), and drops.
//! Long-lived bidirectional streams (TUI, `loopr events tail`) are a
//! future stage's concern.

use std::path::Path;

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

use std::ops::ControlFlow;

use ipc::{
    DaemonEvent, DaemonRequest, DaemonResponse, HandshakeParams, HandshakeResult, IpcMessage, MethodName,
    PROTOCOL_VERSION, WatchFrame, decode_line,
};

use crate::error::LooprError;
use crate::transport::ClientTimeouts;

/// Short-lived IPC client. Owns a `Framed` sink + stream over the Unix
/// socket; drops on exit.
pub struct IpcClient {
    framed: Framed<UnixStream, LinesCodec>,
    // Monotonic per-connection request id. Plain `u64`, not an atomic: every
    // request goes through `&mut self`, so the client is single-threaded by
    // construction and the atomic bought nothing.
    next_id: u64,
    timeouts: ClientTimeouts,
}

impl IpcClient {
    /// Connect to the daemon's socket. Returns `ClientIo` on any
    /// failure; callers that expect "daemon might still be starting up"
    /// should use `transport::connect_or_wait` instead.
    ///
    /// `timeouts` carries the per-call budgets (currently just the
    /// request wall-clock cap). Build via
    /// `ClientTimeouts::from(&config.transport)` to honor operator
    /// overrides, or via `connect_or_wait` (which threads
    /// `TransportSection::default()` for the no-config-file path).
    #[tracing::instrument(name = "client.connect", level = "debug", skip_all, fields(socket = %socket.display()), err)]
    pub async fn connect(socket: &Path, timeouts: ClientTimeouts) -> Result<Self, LooprError> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| LooprError::ClientIo(format!("connect {}: {e}", socket.display())))?;
        let framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
        Ok(Self {
            framed,
            next_id: 1,
            timeouts,
        })
    }

    /// Run the mandatory `system.handshake`. Must be the first request
    /// on every connection per the Stage 3 contract.
    ///
    /// `session_id`, when present, is forwarded to the daemon so every
    /// server-side span emitted for requests on this connection inherits
    /// the caller's session scope (Phase 6 additive extension).
    #[tracing::instrument(
        name = "client.handshake",
        level = "info",
        skip_all,
        fields(session_id = session_id.unwrap_or("")),
        err,
    )]
    pub async fn handshake(&mut self, session_id: Option<&str>) -> Result<HandshakeResult, LooprError> {
        let params = HandshakeParams {
            protocol_version: protocol_version_or_override(),
            session_id: session_id.map(str::to_owned),
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
    #[tracing::instrument(name = "client.request", level = "debug", skip_all, fields(method = ?method), err)]
    pub async fn request(
        &mut self,
        method: MethodName,
        params: serde_json::Value,
    ) -> Result<(DaemonResponse, Vec<DaemonEvent>), LooprError> {
        let method_str: &'static str = method.into();
        self.request_impl(method_str, params).await
    }

    /// Untyped request: escape hatch for methods that have not been
    /// promoted into `MethodName`. The daemon returns
    /// `RpcError::MethodNotFound` for unknown methods, which the caller
    /// maps to whatever error variant fits the surface.
    pub async fn request_raw(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(DaemonResponse, Vec<DaemonEvent>), LooprError> {
        self.request_impl(method, params).await
    }

    /// Open a long-lived subscription to the daemon's live event stream
    /// (Phase 17 of `docs/design/2026-07-11-verified-swarm.md`). Sends
    /// `events.subscribe`, awaits the one-shot ack, then reads frames
    /// forever, invoking `on_frame` for each classified [`WatchFrame`]
    /// (real events, heartbeats, and gap markers).
    ///
    /// This does NOT use `request_impl`'s wall-clock cap: a subscription is
    /// long-lived by design. It returns `Ok(())` when the daemon closes the
    /// stream (shutdown / EOF) or when `on_frame` returns
    /// `ControlFlow::Break`; `Err` only on a wire/codec failure.
    ///
    /// The caller is responsible for racing this against a cancellation
    /// signal (e.g. `tokio::signal::ctrl_c`); dropping the `IpcClient`
    /// closes the socket, which the daemon observes and uses to tear down
    /// its server-side subscription (no leaked forwarder).
    pub async fn subscribe_events<F>(&mut self, mut on_frame: F) -> Result<(), LooprError>
    where
        F: FnMut(WatchFrame) -> ControlFlow<()>,
    {
        let id = self.next_id;
        self.next_id += 1;
        let method: &'static str = MethodName::EventsSubscribe.into();
        let req = DaemonRequest {
            id,
            method: method.to_string(),
            params: serde_json::Value::Null,
        };
        let line =
            serde_json::to_string(&req).map_err(|e| LooprError::ClientIo(format!("serialize subscribe: {e}")))?;
        self.framed
            .send(line)
            .await
            .map_err(|e| LooprError::ClientIo(format!("send subscribe: {e}")))?;

        // Read until the ack (Response whose id matches). Any event frames
        // that interleave before the ack are forwarded, not dropped.
        loop {
            let line = match self.framed.next().await {
                Some(Ok(line)) => line,
                Some(Err(e)) => return Err(LooprError::ClientIo(format!("codec error: {e}"))),
                None => return Ok(()), // daemon closed before ack
            };
            let msg = decode_line(line.as_bytes()).map_err(|e| LooprError::ClientIo(format!("decode: {e}")))?;
            match msg {
                IpcMessage::Response(resp) if resp.id == id => {
                    if let Some(err) = resp.error {
                        return Err(LooprError::Rpc(err));
                    }
                    break; // subscription live
                }
                IpcMessage::Response(other) => {
                    return Err(LooprError::ClientIo(format!(
                        "subscribe: response id mismatch: expected {id}, got {}",
                        other.id
                    )));
                }
                IpcMessage::Event(ev) => {
                    if on_frame(WatchFrame::classify(ev)).is_break() {
                        return Ok(());
                    }
                }
            }
        }

        // Live tail: read frames until the daemon closes the stream or the
        // caller signals stop.
        loop {
            let line = match self.framed.next().await {
                Some(Ok(line)) => line,
                Some(Err(e)) => return Err(LooprError::ClientIo(format!("codec error: {e}"))),
                None => return Ok(()), // daemon shut down / closed the stream
            };
            let msg = decode_line(line.as_bytes()).map_err(|e| LooprError::ClientIo(format!("decode: {e}")))?;
            match msg {
                IpcMessage::Event(ev) => {
                    if on_frame(WatchFrame::classify(ev)).is_break() {
                        return Ok(());
                    }
                }
                // A subscribe stream carries only events after the ack; a
                // stray response is unexpected but non-fatal — skip it.
                IpcMessage::Response(_) => {}
            }
        }
    }

    async fn request_impl(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(DaemonResponse, Vec<DaemonEvent>), LooprError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = DaemonRequest {
            id,
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&req).map_err(|e| LooprError::ClientIo(format!("serialize request: {e}")))?;

        // Wrap both the initial send and the response-collection loop in a
        // single wall-clock cap. Wrapping both directions catches the case
        // where the daemon accepted the connection but its read side has
        // hung (kernel send buffer eventually fills; `framed.send` blocks)
        // as well as the more common dead-daemon-doesn't-reply case.
        let request_budget = self.timeouts.request;
        let framed = &mut self.framed;
        let body = async move {
            framed
                .send(line)
                .await
                .map_err(|e| LooprError::ClientIo(format!("send: {e}")))?;

            // Collect messages until we get the response whose id matches.
            // Broadcast events from the daemon interleave with responses; we
            // buffer them for the caller (Stage 4's callers ignore them).
            let mut events: Vec<DaemonEvent> = Vec::new();
            loop {
                let line = match framed.next().await {
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
        };
        match tokio::time::timeout(request_budget, body).await {
            Ok(res) => res,
            Err(_elapsed) => Err(LooprError::ClientIo(format!(
                "request timed out after {}s",
                request_budget.as_secs()
            ))),
        }
    }
}

/// Protocol-version advertisement with a test-only override hook.
///
/// Release builds ALWAYS return `ipc::PROTOCOL_VERSION`. Debug builds
/// (and `#[cfg(test)]`) read `LOOPR_PROTOCOL_VERSION_OVERRIDE` first, so
/// the acceptance test for version-mismatch rejection can exercise the
/// unhappy path without a second binary build.
pub fn protocol_version_or_override() -> u32 {
    if cfg!(debug_assertions)
        && let Ok(v) = std::env::var("LOOPR_PROTOCOL_VERSION_OVERRIDE")
        && let Ok(n) = v.parse::<u32>()
    {
        return n;
    }
    PROTOCOL_VERSION
}

#[cfg(test)]
mod tests;
