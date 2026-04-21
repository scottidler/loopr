//! Exhaustive `ipc::Method` dispatch.
//!
//! Compile-error-driven: adding a variant to `ipc::Method` in a future
//! stage produces a non-exhaustive-match error here. Stage 5 resolves
//! that by adding an arm for `Method::PlanCreate`; the exhaustive match
//! is the mechanism that forces the pair.

use tracing::{info, warn};

use ipc::{
    DaemonRequest, DaemonResponse, HandshakeParams, HandshakeResult, Method, PROTOCOL_VERSION, PlanCreateParams,
    PlanCreateResult, PlanListResult, RpcError, StatusResult,
};
use store::StoreError;

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
        Method::PlanCreate(params) => handle_plan_create(req.id, params, ctx).await,
        Method::PlanList => handle_plan_list(req.id, ctx).await,
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

async fn handle_plan_create(id: u64, params: PlanCreateParams, ctx: &DaemonContext) -> DaemonResponse {
    let plan = domain::Plan::new(params.goal);
    let plan_snapshot = plan.clone();
    if let Err(e) = ctx.store.plans().create(plan.clone()).await {
        warn!(request_id = id, error = %e, "plan.create failed at store");
        return DaemonResponse::err(id, map_store_error(e));
    }

    // Stage 6: decompose the Plan into Works and persist them. On
    // decomposer error the Plan remains persisted (scope memo A+2:
    // reconcile-on-restart is Stage 7's problem); we log and return
    // Plan success so the user at least sees their Plan landed.
    match decomposer::decompose(&plan_snapshot, &ctx.target, &*ctx.llm).await {
        Ok(works) => {
            let count = works.len();
            match ctx.store.works().create_many(works).await {
                Ok(ids) => {
                    info!(request_id = id, plan_id = %plan_snapshot.id, work_count = count, ids = ?ids, "plan.create decomposed + persisted")
                }
                Err(e) => warn!(
                    request_id = id,
                    plan_id = %plan_snapshot.id,
                    error = %e,
                    "plan.create persisted Plan but works.create_many failed; Stage 7 reconcile"
                ),
            }
        }
        Err(e) => {
            warn!(
                request_id = id,
                plan_id = %plan_snapshot.id,
                error = %e,
                "plan.create persisted Plan but decomposer failed; Stage 7 reconcile"
            );
        }
    }

    let result = PlanCreateResult { plan: plan_snapshot };
    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(id, v),
        Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize plan.create: {e}"))),
    }
}

async fn handle_plan_list(id: u64, ctx: &DaemonContext) -> DaemonResponse {
    match ctx.store.plans().list().await {
        Ok(plans) => {
            let result = PlanListResult { plans };
            match serde_json::to_value(&result) {
                Ok(v) => DaemonResponse::ok(id, v),
                Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize plan.list: {e}"))),
            }
        }
        Err(e) => {
            warn!(request_id = id, error = %e, "plan.list failed at store");
            DaemonResponse::err(id, map_store_error(e))
        }
    }
}

/// Translate `store::StoreError` into a wire-level `RpcError`. Kept in the
/// handler crate (not in `store`) so the anti-corruption boundary is
/// explicit: the protocol chooses which internal errors surface as which
/// RPC codes and how their messages are shaped.
fn map_store_error(err: StoreError) -> RpcError {
    match err {
        StoreError::RecordNotFound { collection, id } => RpcError::NotFound(format!("{collection}/{id}")),
        StoreError::AlreadyExists { collection, id } => {
            RpcError::InvalidRequest(format!("already exists: {collection}/{id}"))
        }
        StoreError::Io(msg) => RpcError::Internal(format!("store io: {msg}")),
        StoreError::Corruption(msg) => RpcError::Internal(format!("store corruption: {msg}")),
        StoreError::Serde(msg) => RpcError::Internal(format!("store serde: {msg}")),
    }
}

#[cfg(test)]
mod tests;
