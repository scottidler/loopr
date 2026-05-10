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
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};
use tracing::{Instrument, info, warn};

use ipc::{DaemonResponse, RpcError, decode_request_line};

use llm::LlmClient;

use crate::daemon::DaemonContext;
use crate::error::LooprError;
use crate::transport::handler::{self, HandshakeState};

/// Ceiling on how long the accept loop waits for in-flight handler tasks
/// to finish after shutdown is observed. Matches the daemon-stop
/// SIGTERM-to-SIGKILL escalation window: a handler that has not yielded
/// in this long is wedged by the same definition that motivates escalating
/// signal strength. Handlers still outstanding when the window expires
/// are `JoinSet::abort_all`'d so shutdown does not deadlock and the
/// `Arc<DaemonContext>` clones they hold can be released before
/// `daemon::run_active_daemon` calls `Arc::try_unwrap` to close the store.
pub const HANDLER_DRAIN_TIMEOUT_SECS: u64 = 10;

/// Bind a `UnixListener` at `socket`. Caller is responsible for removing
/// any pre-existing file at the path (the daemon's active-phase does that
/// unconditionally; see `daemon::run_active_daemon`).
pub fn bind_listener(socket: &Path) -> Result<UnixListener, LooprError> {
    UnixListener::bind(socket).map_err(|e| LooprError::DaemonStartup(format!("bind {}: {e}", socket.display())))
}

/// Daemon accept loop. Exits when `ctx.shutting_down` is set and the
/// shutdown notify wakes us. Spawns one `handle_client` task per
/// incoming connection into a `JoinSet` so shutdown can drain their
/// `Arc<DaemonContext>` clones before the daemon tries to close the
/// store.
///
/// **Shutdown sequence:** on shutdown, await `join_next` with a bounded
/// timeout so handlers that have already observed shutdown themselves
/// exit cleanly; any still outstanding after the timeout are
/// `abort_all`'d and reaped. Either path guarantees that by the time
/// this function returns, every spawned handler has released its
/// `Arc<DaemonContext>` clone.
pub async fn accept_loop<L>(listener: UnixListener, ctx: Arc<DaemonContext<L>>) -> Result<(), LooprError>
where
    L: LlmClient + Send + Sync + 'static,
{
    let mut handlers: JoinSet<()> = JoinSet::new();

    loop {
        // Reap completed handlers non-blockingly so the JoinSet does not
        // grow unbounded under sustained connection churn and so their
        // `Arc<DaemonContext>` clones release as soon as their tasks end.
        while let Some(res) = handlers.try_join_next() {
            if let Err(e) = res
                && !e.is_cancelled()
            {
                warn!("handler task panicked: {e}");
            }
        }

        // Loop-top atomic check: `Notify::notify_waiters` is non-sticky,
        // so if the signal watcher fires during the microseconds between
        // the last `select!` return and the next iteration, the wakeup
        // would be lost and `listener.accept()` could block forever. The
        // atomic load catches that case.
        //
        // `Relaxed` is sufficient: the flag carries no happens-before
        // relationship with other data. The paired `Notify` wakes
        // tasks that race with the load; no read or write depends on
        // observing the flag before another memory location.
        if ctx.shutting_down.load(Ordering::Relaxed) {
            info!("accept_loop: shutdown flag observed");
            break;
        }
        tokio::select! {
            biased;
            _ = ctx.shutdown_notify.notified() => {
                info!("accept_loop: shutdown notified");
                break;
            }
            accept = listener.accept() => {
                let (stream, _) = accept
                    .map_err(|e| LooprError::ClientIo(format!("accept: {e}")))?;
                let conn_id = uuid::Uuid::new_v4();
                let span = tracing::info_span!(
                    "ipc.connection",
                    conn_id = %conn_id,
                    client_session_id = tracing::field::Empty,
                );
                let ctx_cloned = ctx.clone();
                handlers.spawn(handle_client(stream, ctx_cloned).instrument(span));
            }
        }
    }

    let remaining = handlers.len();
    if remaining > 0 {
        info!("accept_loop: draining {remaining} in-flight handler(s)");
    }
    let drain = async {
        while let Some(res) = handlers.join_next().await {
            if let Err(e) = res
                && !e.is_cancelled()
            {
                warn!("handler task panicked during shutdown drain: {e}");
            }
        }
    };
    match timeout(Duration::from_secs(HANDLER_DRAIN_TIMEOUT_SECS), drain).await {
        Ok(()) => info!("accept_loop: all handlers drained"),
        Err(_) => {
            warn!(
                "accept_loop: drain timed out after {HANDLER_DRAIN_TIMEOUT_SECS}s; aborting {} handler(s)",
                handlers.len()
            );
            handlers.abort_all();
            while handlers.join_next().await.is_some() {}
        }
    }
    Ok(())
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
pub async fn handle_client<L>(stream: UnixStream, ctx: Arc<DaemonContext<L>>)
where
    L: LlmClient + Send + Sync + 'static,
{
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    let mut event_rx = ctx.events.subscribe();
    let mut state = HandshakeState::Pending;

    // Read-side idle timer. Hoisted outside the loop and `Pin::reset` only
    // when `framed.next()` yields real client traffic — broadcasts on
    // `event_rx` deliberately do NOT reset it (otherwise a chatty daemon
    // would keep a silent client alive forever). See
    // docs/design/2026-05-09-ipc-timeouts.md Phase 3.
    let server_idle = ctx.server_timeouts.idle;
    let server_write = ctx.server_timeouts.write;
    let mut idle = Box::pin(tokio::time::sleep(server_idle));

    loop {
        // Fast loop-top shutdown check (same reason as `accept_loop`).
        if ctx.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        tokio::select! {
            biased;
            _ = ctx.shutdown_notify.notified() => return,
            _ = &mut idle => {
                warn!(server_idle_secs = server_idle.as_secs(), "server idle timeout exceeded; closing connection");
                return;
            }
            line = framed.next() => match line {
                Some(Ok(line)) => {
                    // Real client traffic — reset the read-idle timer.
                    idle.as_mut().reset(tokio::time::Instant::now() + server_idle);
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
                            // Bound the response-write so a SIGSTOPped /
                            // stalled-reader client whose socket buffer has
                            // filled cannot pin this task forever. See
                            // docs/design/2026-05-09-ipc-timeouts.md Phase 3b.
                            match timeout(server_write, framed.send(line_out)).await {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => {
                                    warn!("response send failed: {e}; closing connection");
                                    return;
                                }
                                Err(_elapsed) => {
                                    warn!(
                                        server_write_secs = server_write.as_secs(),
                                        "server write timeout exceeded (response); closing connection"
                                    );
                                    return;
                                }
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
                    // AC 20: discriminate the oversize-frame path from
                    // generic codec failures so the log message matches
                    // the Stage 4 design. `LinesCodec` enforces the
                    // 1-MiB limit at its own layer (before bytes reach
                    // ipc::decode_request_line), so the "LineTooLong"
                    // marker lives here, not in ipc::ParseError.
                    match e {
                        LinesCodecError::MaxLineLengthExceeded => {
                            warn!(
                                "ParseError::LineTooLong (line exceeds MAX_LINE_BYTES={}); closing connection",
                                ipc::MAX_LINE_BYTES
                            );
                        }
                        LinesCodecError::Io(io_err) => {
                            warn!("codec error: {io_err}; closing connection");
                        }
                    }
                    return;
                }
                None => return, // client disconnected
            },
            event = event_rx.recv() => match event {
                // NOTE: `idle` is intentionally NOT reset here. Broadcasts
                // are server-originated and do not constitute proof of life
                // on the read side; resetting on broadcast would let a
                // chatty daemon hold a silent or SIGSTOPped client open
                // indefinitely.
                Ok(event) => {
                    let line_out = match serde_json::to_string(&event) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("event serialize failed: {e}; dropping");
                            continue;
                        }
                    };
                    // Bound the broadcast-write for the same reason as the
                    // response-write above: a stalled reader's full send
                    // buffer would otherwise pin this task forever.
                    match timeout(server_write, framed.send(line_out)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => return,
                        Err(_elapsed) => {
                            warn!(
                                server_write_secs = server_write.as_secs(),
                                "server write timeout exceeded (event); closing connection"
                            );
                            return;
                        }
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
