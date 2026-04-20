//! Exhaustive `ipc::Method` dispatch.
//!
//! Compile-error-driven: adding a variant to `ipc::Method` in a future
//! stage produces a non-exhaustive-match error here. Stage 5 resolves
//! that by adding an arm for `Method::PlanCreate`; the exhaustive match
//! is the mechanism that forces the pair.

use tracing::warn;

use ipc::{
    DaemonRequest, DaemonResponse, HandshakeParams, HandshakeResult, Method, PROTOCOL_VERSION, RpcError, StatusResult,
};

use crate::daemon::{DAEMON_VERSION, DaemonContext};

/// Per-connection handshake state. Every new connection starts `Pending`;
/// after the client sends `system.handshake` with a matching protocol
/// version the state flips to `Complete`. Any other method received while
/// `Pending` is rejected with `InvalidRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeState {
    Pending,
    Complete,
}

/// Dispatch a single request to the correct handler. Returns the
/// `DaemonResponse` to be sent back on the wire. Does not perform I/O
/// beyond reading `ctx` fields and allocating the response.
pub async fn dispatch(req: &DaemonRequest, state: &mut HandshakeState, ctx: &DaemonContext) -> DaemonResponse {
    let method = match Method::try_from(req) {
        Ok(m) => m,
        Err(rpc_err) => {
            warn!(
                request_id = req.id,
                method = %req.method,
                error = %rpc_err,
                "rejected: method-not-found or invalid-params"
            );
            return DaemonResponse::err(req.id, rpc_err);
        }
    };

    // Handshake ordering enforcement (Stage 3 contract).
    if matches!(state, HandshakeState::Pending) && !matches!(method, Method::Handshake(_)) {
        warn!(
            request_id = req.id,
            method = %req.method,
            "rejected: handshake required first"
        );
        return DaemonResponse::err(
            req.id,
            RpcError::InvalidRequest(format!("handshake required before: {}", req.method)),
        );
    }

    // Exhaustive match: adding a variant to ipc::Method causes a compile
    // error here. Stage 5+ adds arms alongside the ipc::Method additions.
    match method {
        Method::Handshake(params) => handle_handshake(req.id, params, state),
        Method::Status => handle_status(req.id, ctx),
    }
}

fn handle_handshake(id: u64, params: HandshakeParams, state: &mut HandshakeState) -> DaemonResponse {
    if params.protocol_version != PROTOCOL_VERSION {
        warn!(
            request_id = id,
            client_version = params.protocol_version,
            daemon_version = PROTOCOL_VERSION,
            "handshake: protocol version mismatch"
        );
        return DaemonResponse::err(
            id,
            RpcError::protocol_version_mismatch(params.protocol_version, PROTOCOL_VERSION),
        );
    }
    *state = HandshakeState::Complete;
    let result = HandshakeResult {
        protocol_version: PROTOCOL_VERSION,
        daemon_version: DAEMON_VERSION.to_string(),
    };
    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(id, v),
        Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize handshake: {e}"))),
    }
}

fn handle_status(id: u64, ctx: &DaemonContext) -> DaemonResponse {
    // Stage 4 has no records to count; active_plans / active_works are
    // hardcoded zeros. Stage 5+ reads from taskstore via a new dep.
    let result = StatusResult {
        started_at: ctx.started_at.to_rfc3339(),
        pid: ctx.pid,
        active_plans: 0,
        active_works: 0,
    };
    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(id, v),
        Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize status: {e}"))),
    }
}

#[cfg(test)]
mod tests;
