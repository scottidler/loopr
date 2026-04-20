//! Daemon-side transport: socket bind, accept loop, per-connection
//! `handle_client` tasks.
//!
//! The accept loop spawns one handler task per connection. Each handler
//! owns a `Framed<UnixStream, LinesCodec>` sized to `ipc::MAX_LINE_BYTES`
//! and drives its own per-connection `HandshakeState`. Broadcast events
//! forward to every connected client via `ctx.events` (Stage 4 never
//! emits; capacity is future-proofing for Stage 5+).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio_util::codec::{Framed, LinesCodec};
use tracing::{Instrument, info, warn};

use ipc::{DaemonResponse, RpcError, decode_request_line};

use crate::daemon::DaemonContext;
use crate::error::LooprError;
use crate::transport::handler::{self, HandshakeState};

/// Bind a `UnixListener` at `socket`. Caller is responsible for removing
/// any pre-existing file at the path (the daemon's active-phase does that
/// unconditionally; see `daemon::run_active_daemon`).
pub fn bind_listener(socket: &Path) -> Result<UnixListener, LooprError> {
    UnixListener::bind(socket).map_err(|e| LooprError::DaemonStartup(format!("bind {}: {e}", socket.display())))
}

/// Daemon accept loop. Exits when `ctx.shutting_down` is set and the
/// shutdown notify wakes us. Spawns one `handle_client` task per
/// incoming connection.
pub async fn accept_loop(listener: UnixListener, ctx: Arc<DaemonContext>) -> Result<(), LooprError> {
    loop {
        // Loop-top atomic check: `Notify::notify_waiters` is non-sticky,
        // so if the signal watcher fires during the microseconds between
        // the last `select!` return and the next iteration, the wakeup
        // would be lost and `listener.accept()` could block forever. The
        // atomic load catches that case.
        if ctx.shutting_down.load(Ordering::SeqCst) {
            info!("accept_loop: shutdown flag observed");
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = ctx.shutdown_notify.notified() => {
                info!("accept_loop: shutdown notified");
                return Ok(());
            }
            accept = listener.accept() => {
                let (stream, _) = accept
                    .map_err(|e| LooprError::ClientIo(format!("accept: {e}")))?;
                let conn_id = uuid::Uuid::new_v4();
                let span = tracing::info_span!("ipc.connection", conn_id = %conn_id);
                let ctx = ctx.clone();
                tokio::spawn(handle_client(stream, ctx).instrument(span));
            }
        }
    }
}

/// Per-connection handler. Reads NDJSON request lines, dispatches each
/// through `handler::dispatch`, writes the response. Also forwards any
/// broadcast `DaemonEvent`s to the connected client.
///
/// The encoding side uses `serde_json::to_string` + `framed.send(&str)`;
/// `LinesCodec` appends its own `\n`, so feeding it `ipc::encode_line`'s
/// already-terminated output would produce a double newline on the wire.
/// See §API Design note "Note on `ipc::encode_line` vs. `serde_json::
/// to_string` + `framed.send`" in the Stage 4 design doc.
pub async fn handle_client(stream: UnixStream, ctx: Arc<DaemonContext>) {
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    let mut event_rx = ctx.events.subscribe();
    let mut state = HandshakeState::Pending;

    loop {
        // Fast loop-top shutdown check (same reason as `accept_loop`).
        if ctx.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        tokio::select! {
            biased;
            _ = ctx.shutdown_notify.notified() => return,
            line = framed.next() => match line {
                Some(Ok(line)) => {
                    match decode_request_line(line.as_bytes()) {
                        Ok(req) => {
                            info!(
                                request_id = req.id,
                                method = %req.method,
                                peer = "client",
                                "request received"
                            );
                            let response = handler::dispatch(&req, &mut state, &ctx).await;
                            let close_after = is_protocol_version_mismatch(&response);
                            let line_out = match serde_json::to_string(&response) {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!("response serialize failed: {e}; closing connection");
                                    return;
                                }
                            };
                            if framed.send(line_out).await.is_err() {
                                return;
                            }
                            if close_after {
                                // Forward-compatibility: close on version
                                // mismatch so the client cannot retry on
                                // the same connection with a different
                                // advertisement.
                                return;
                            }
                        }
                        Err(e) => {
                            // No fake-id synthesis (Stage 3 contract).
                            warn!("parse error: {e}; closing connection");
                            return;
                        }
                    }
                }
                Some(Err(e)) => {
                    warn!("codec error: {e}; closing connection");
                    return;
                }
                None => return, // client disconnected
            },
            event = event_rx.recv() => match event {
                Ok(event) => {
                    let line_out = match serde_json::to_string(&event) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("event serialize failed: {e}; dropping");
                            continue;
                        }
                    };
                    if framed.send(line_out).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

fn is_protocol_version_mismatch(resp: &DaemonResponse) -> bool {
    matches!(&resp.error, Some(RpcError::ProtocolVersionMismatch(_)))
}

#[cfg(test)]
mod tests;
