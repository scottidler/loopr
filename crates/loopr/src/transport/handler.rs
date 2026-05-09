//! Exhaustive `ipc::Method` dispatch.
//!
//! Compile-error-driven: adding a variant to `ipc::Method` in a future
//! stage produces a non-exhaustive-match error here. Stage 5 resolves
//! that by adding an arm for `Method::PlanCreate`; the exhaustive match
//! is the mechanism that forces the pair.

use std::sync::Arc;

use tracing::{info, instrument, warn};

use agents::{DirectorDeps, DirectorError, run_director};
use ipc::{
    BundleSummary, DaemonRequest, DaemonResponse, HandshakeParams, HandshakeResult, Method, PROTOCOL_VERSION,
    PlanCreateParams, PlanCreateResult, PlanSummary, RecordGetParams, RecordKind, RecordListParams, RecordResult,
    RecordsResult, RpcError, StatusResult, TickSummary, WorkSummary,
};
use llm::LlmClient;
use store::StoreError;

use crate::daemon::context::DaemonSpawner;
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
#[instrument(
    name = "ipc.dispatch",
    level = "info",
    skip_all,
    fields(request_id = req.id, method = %req.method, handshake_state = ?state),
)]
pub async fn dispatch<L>(req: &DaemonRequest, state: &mut HandshakeState, ctx: &Arc<DaemonContext<L>>) -> DaemonResponse
where
    L: LlmClient + Send + Sync + 'static,
{
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
        Method::RecordList(params) => handle_record_list(req.id, params, ctx).await,
        Method::RecordGet(params) => handle_record_get(req.id, params, ctx).await,
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
    // Phase 6: record the caller's session-id on the current span (the
    // enclosing `ipc.connection` span in `transport::server`). Every event
    // emitted on this connection inherits `client_session_id` for easier
    // correlation across the daemon's log and the client's log. Absent
    // session-id means the caller did not advertise one (legacy client or
    // daemon-internal call) — in that case we omit the field rather than
    // recording an empty placeholder.
    if let Some(ref sid) = params.session_id {
        tracing::Span::current().record("client_session_id", sid.as_str());
        tracing::info!(request_id = id, client_session_id = %sid, "handshake: client session attached");
    } else {
        tracing::info!(request_id = id, "handshake: client did not advertise session_id");
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

#[instrument(name = "ipc.status", level = "debug", skip_all, fields(request_id = id))]
fn handle_status<L: LlmClient + Send + Sync + 'static>(id: u64, ctx: &Arc<DaemonContext<L>>) -> DaemonResponse {
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

#[instrument(
    name = "ipc.plan_create",
    level = "info",
    skip_all,
    fields(request_id = id, goal_len = params.goal.len(), plan_id = tracing::field::Empty),
)]
async fn handle_plan_create<L>(id: u64, params: PlanCreateParams, ctx: &Arc<DaemonContext<L>>) -> DaemonResponse
where
    L: LlmClient + Send + Sync + 'static,
{
    // Stage 8 wiring: construct Plan first (generates PlanId in memory) so
    // we have the branch name before any store write. Then create the
    // integration branch BEFORE persisting anything; if git fails we bail
    // without leaving orphan Plan/Work records on disk. Plan::new births
    // PlanStatus::Active, so no Draft->Active transition is needed here.
    let plan = domain::Plan::new(params.goal);
    let plan_snapshot = plan.clone();
    tracing::Span::current().record("plan_id", plan.id.to_string().as_str());

    if let Err(e) = crate::daemon::git::ensure_integration_branch(&ctx.target, &plan.id).await {
        warn!(request_id = id, plan_id = %plan.id, error = %e, "plan.create failed at integration-branch creation");
        return DaemonResponse::err(
            id,
            RpcError::Internal(format!("integration branch creation failed: {e}")),
        );
    }

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
            match ctx.store.works().create_many(works.clone()).await {
                Ok(ids) => {
                    // Dep gate: partition into unblocked (all deps Done or
                    // no deps) and held (at least one dep not Done). Only
                    // unblocked Works get an Implementer spawned immediately;
                    // held Works stay Pending and are promoted reactively when
                    // their deps reach Done (see promote_unblocked_siblings).
                    let (unblocked, held): (Vec<_>, Vec<_>) = works.iter().partition(|w| w.all_deps_done(&works));
                    let unblocked_count = unblocked.len();
                    let held_count = held.len();
                    // Warn on any held Work whose dep ids are not in this
                    // batch - indicates a decomposer bug (unknown dep ref).
                    for w in &held {
                        for dep_id in &w.dependencies {
                            if !works.iter().any(|s| &s.id == dep_id) {
                                warn!(
                                    work_id = %w.id,
                                    dep_id = %dep_id,
                                    "dep_gate: unknown dep id not in decomposed batch; Work may hang Pending"
                                );
                            }
                        }
                    }
                    info!(
                        request_id = id,
                        plan_id = %plan_snapshot.id,
                        work_count = count,
                        ids = ?ids,
                        unblocked = unblocked_count,
                        held = held_count,
                        "plan.create decomposed + persisted"
                    );
                    let mut tasks = ctx.implementer_tasks.lock().await;
                    for work in unblocked {
                        let task_ctx = Arc::clone(ctx);
                        tasks.spawn(task_ctx.spawn_implementer_for_work(work.clone()));
                    }
                    drop(tasks);
                    spawn_director_for_plan(ctx, plan_snapshot.id.clone()).await;
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

/// Handle `record.list`. Fetches the full records for the requested kind
/// from the store, then projects each into its summary type so the
/// response stays well under the 1 MiB IPC frame cap. See
/// `docs/design/2026-04-23-cli-plumbing-shape.md`.
#[instrument(
    name = "ipc.record_list",
    level = "debug",
    skip_all,
    fields(request_id = id, kind = ?params.kind),
)]
async fn handle_record_list<L: LlmClient + Send + Sync + 'static>(
    id: u64,
    params: RecordListParams,
    ctx: &Arc<DaemonContext<L>>,
) -> DaemonResponse {
    let result = match params.kind {
        RecordKind::Plan => match ctx.store.plans().list().await {
            Ok(plans) => RecordsResult::Plans(plans.iter().map(PlanSummary::from).collect()),
            Err(e) => return store_err_response(id, e, "plans"),
        },
        RecordKind::Work => match ctx.store.works().list().await {
            Ok(works) => RecordsResult::Works(works.iter().map(WorkSummary::from).collect()),
            Err(e) => return store_err_response(id, e, "works"),
        },
        RecordKind::Bundle => match ctx.store.bundles().list().await {
            Ok(bundles) => RecordsResult::Bundles(bundles.iter().map(BundleSummary::from).collect()),
            Err(e) => return store_err_response(id, e, "bundles"),
        },
        RecordKind::Tick => match ctx.store.ticks().list().await {
            Ok(ticks) => RecordsResult::Ticks(ticks.iter().map(TickSummary::from).collect()),
            Err(e) => return store_err_response(id, e, "ticks"),
        },
    };
    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(id, v),
        Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize record.list: {e}"))),
    }
}

/// Handle `record.get`. Routes by id prefix to the right store accessor
/// and returns the full record wrapped in `RecordResult`. The prefix
/// literals mirror the `$prefix` arguments to the `id_type!` macro
/// invocations in `crates/domain/src/id.rs`.
#[instrument(
    name = "ipc.record_get",
    level = "debug",
    skip_all,
    fields(request_id = id, record_id = %params.id),
)]
async fn handle_record_get<L: LlmClient + Send + Sync + 'static>(
    id: u64,
    params: RecordGetParams,
    ctx: &Arc<DaemonContext<L>>,
) -> DaemonResponse {
    use std::str::FromStr;
    let prefix = params.id.split('-').next().unwrap_or("");
    let result = match prefix {
        "pl" => match domain::PlanId::from_str(&params.id) {
            Ok(pid) => match ctx.store.plans().get(&pid).await {
                Ok(plan) => RecordResult::Plan(plan),
                Err(e) => return store_err_response(id, e, "plans"),
            },
            Err(never) => match never {},
        },
        "wk" => match domain::WorkId::from_str(&params.id) {
            Ok(wid) => match ctx.store.works().get(&wid).await {
                Ok(work) => RecordResult::Work(work),
                Err(e) => return store_err_response(id, e, "works"),
            },
            Err(never) => match never {},
        },
        "bd" => match domain::BundleId::from_str(&params.id) {
            Ok(bid) => match ctx.store.bundles().get(&bid).await {
                Ok(bundle) => RecordResult::Bundle(bundle),
                Err(e) => return store_err_response(id, e, "bundles"),
            },
            Err(never) => match never {},
        },
        "tk" => match domain::TickId::from_str(&params.id) {
            Ok(tid) => match ctx.store.ticks().get(&tid).await {
                Ok(tick) => RecordResult::Tick(tick),
                Err(e) => return store_err_response(id, e, "ticks"),
            },
            Err(never) => match never {},
        },
        _ => {
            return DaemonResponse::err(
                id,
                RpcError::InvalidParams(format!(
                    "unknown id prefix in `{}`; expected one of: pl-, wk-, bd-, tk-",
                    params.id
                )),
            );
        }
    };
    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(id, v),
        Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize record.get: {e}"))),
    }
}

fn store_err_response(id: u64, err: StoreError, collection: &'static str) -> DaemonResponse {
    warn!(request_id = id, collection = collection, error = %err, "record query failed at store");
    DaemonResponse::err(id, map_store_error(err))
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
        StoreError::Stale { expected, actual } => {
            RpcError::InvalidRequest(format!("stale record: expected updated_at={expected}, actual={actual}"))
        }
        StoreError::DuplicateTick {
            tick_id,
            plan_id,
            bundles,
        } => RpcError::InvalidRequest(format!(
            "duplicate tick: existing tick_id={tick_id} for plan_id={plan_id} with bundles={bundles:?}"
        )),
    }
}

/// Spawn a Director task into `ctx.director_tasks` for the given Plan.
/// Director Phase 3 wiring: every newly-created Plan gets a per-Plan
/// Director task that polls TaskStore, accepts Reviewed Bundles, and
/// recovers Blocked Works. Errors at task-exit emit `error!`; the
/// Director's restart logic and lifeguard handle recoverable failures
/// in-task.
async fn spawn_director_for_plan<L>(ctx: &Arc<DaemonContext<L>>, plan_id: domain::PlanId)
where
    L: LlmClient + Send + Sync + 'static,
{
    if ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
        warn!(plan_id = %plan_id, "shutdown in progress; skipping Director spawn");
        return;
    }
    let deps = DirectorDeps {
        llm: Arc::clone(&ctx.llm),
        store: Arc::clone(&ctx.store),
        context: Arc::clone(&ctx.context_builder),
        spawner: DaemonSpawner(Arc::clone(ctx)),
        config: ctx.director_config.clone(),
        shutdown: Arc::clone(&ctx.shutdown_notify),
    };
    let mut directors = ctx.director_tasks.lock().await;
    let plan_id_for_log = plan_id.clone();
    directors.spawn(async move {
        match run_director(&plan_id, &deps).await {
            Ok(()) => info!(plan_id = %plan_id_for_log, "director task exited Ok"),
            Err(DirectorError::NeedHelp(reason)) => warn!(
                plan_id = %plan_id_for_log,
                reason = %reason,
                "director exited with NeedHelp"
            ),
            Err(e) => tracing::error!(plan_id = %plan_id_for_log, error = %e, "director exited with error"),
        }
    });
}

#[cfg(test)]
mod tests;
