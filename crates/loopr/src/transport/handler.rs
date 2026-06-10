//! Exhaustive `ipc::Method` dispatch.
//!
//! Compile-error-driven: adding a variant to `ipc::Method` in a future
//! stage produces a non-exhaustive-match error here. Stage 5 resolves
//! that by adding an arm for `Method::PlanCreate`; the exhaustive match
//! is the mechanism that forces the pair.

use std::sync::Arc;

use tracing::{debug, info, instrument, warn};

use agents::{DirectorDeps, DirectorError, run_director};
use futures_util::FutureExt;
use ipc::{
    BundleSummary, DIRECTOR_CHAT_MESSAGE_BYTE_CAP, DaemonRequest, DaemonResponse, DirectorChatParams,
    DirectorChatResult, DirectorStatusParams, DirectorStatusResult, DirectorStatusSnapshot, HandshakeParams,
    HandshakeResult, Method, PROTOCOL_VERSION, PlanCreateParams, PlanCreateResult, PlanOverrideParams,
    PlanOverrideResult, PlanSummary, RecordGetParams, RecordKind, RecordListParams, RecordResult, RecordsResult,
    RpcError, StatusResult, TickSummary, WorkSummary,
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
        Method::DirectorChat(params) => handle_director_chat(req.id, params, ctx).await,
        Method::PlanOverride(params) => handle_plan_override(req.id, params, ctx).await,
        Method::DirectorStatus(params) => handle_director_status(req.id, params, ctx).await,
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

    // Phase C: only create the per-Plan integration branch in the default
    // (branch) mode. Under the no-branch override the Integrator merges
    // onto the checked-out branch, so there is nothing to create here.
    if ctx.integrator_config.integration_branch
        && let Err(e) = crate::daemon::git::ensure_integration_branch(&ctx.target, &plan.id).await
    {
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

    // Phase B (async ACK): the Plan is persisted, so the client can be
    // told its id now. Decompose (an ~18s LLM call) + Works persist + the
    // initial Implementer/Director spawns run on a daemon-owned task in
    // `plan_create_tasks`, drained FIRST at shutdown (root of the spawn
    // DAG). This restores Stage 4's ACK-and-exit intent: the client no
    // longer blocks on decompose and never trips the request timeout on a
    // successful Plan.
    {
        let task_ctx = Arc::clone(ctx);
        let plan_for_task = plan_snapshot.clone();
        ctx.plan_create_tasks.lock().await.spawn(async move {
            if task_ctx.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            decompose_and_dispatch(&task_ctx, plan_for_task, id).await;
        });
    }

    let result = PlanCreateResult { plan: plan_snapshot };
    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(id, v),
        Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize plan.create: {e}"))),
    }
}

/// Decompose a persisted Plan into Works, persist them, and spawn the
/// initial Implementers + the per-Plan Director. Runs on a
/// `plan_create_tasks` task (Phase B async ACK), so it carries its own
/// span — `tokio::spawn` detaches it from the handler's span.
///
/// On decomposer error the Plan remains persisted (scope memo A+2:
/// reconcile-on-restart is Stage 7's problem); we log and return, leaving
/// an `Active` Plan with zero Works and no children — the same non-fatal
/// outcome the synchronous path had, now observed in logs + on-disk
/// rather than the (already-sent) client response.
#[tracing::instrument(level = "info", skip_all, fields(request_id, plan_id = %plan.id))]
async fn decompose_and_dispatch<L>(ctx: &Arc<DaemonContext<L>>, mut plan: domain::Plan, request_id: u64)
where
    L: LlmClient + Send + Sync + 'static,
{
    match decomposer::decompose(&plan, &ctx.target, &*ctx.llm).await {
        Ok(works) => {
            let count = works.len();
            // F4: persist with collision re-minting. `create_many` now
            // rejects an id that collides with an earlier Plan's Work; on
            // collision we re-mint the batch (remapping the dep edges) and
            // retry, then proceed with the (possibly re-minted) works.
            let (works, ids) = match persist_works_with_remint(&ctx.store, works).await {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(
                        request_id,
                        plan_id = %plan.id,
                        error = %e,
                        "plan.create persisted Plan but works.create_many failed; Plan -> Stalled"
                    );
                    stall_plan_after_decompose_failure(ctx, &mut plan).await;
                    return;
                }
            };
            // Dep gate: partition into unblocked (all deps Done or
            // no deps) and held (at least one dep not Done). Only
            // unblocked Works get an Implementer spawned immediately;
            // held Works stay Pending and are promoted reactively when
            // their deps reach Done (see promote_unblocked_siblings).
            let graph = domain::WorkGraph::from_works(&works);
            let done: std::collections::HashSet<domain::WorkId> = works
                .iter()
                .filter(|w| w.status == domain::WorkStatus::Done)
                .map(|w| w.id.clone())
                .collect();
            let ready: std::collections::HashSet<domain::WorkId> = graph.ready_set(&done).into_iter().collect();
            let (unblocked, held): (Vec<_>, Vec<_>) = works.iter().partition(|w| ready.contains(&w.id));
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
                request_id,
                plan_id = %plan.id,
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
            spawn_director_for_plan(ctx, plan.id.clone()).await;
        }
        Err(e) => {
            warn!(
                request_id,
                plan_id = %plan.id,
                error = %e,
                "plan.create persisted Plan but decomposer failed; Plan -> Stalled"
            );
            stall_plan_after_decompose_failure(ctx, &mut plan).await;
        }
    }
}

/// Persist a decomposed Work batch, re-minting ids on collision (F4).
///
/// `WorkId`s are 5-char base36 (~60.4M space), so a fresh decomposition's
/// ids can collide with an earlier Plan's persisted Work. `create_many`
/// now rejects such a collision with `AlreadyExists` (it would otherwise
/// `INSERT OR REPLACE`-overwrite the earlier Work). On collision we
/// re-mint EVERY id in the batch and remap the freshly-built dependency
/// edges (which reference the ids we just minted), then retry. Returns the
/// (possibly re-minted) works alongside their persisted ids so the caller's
/// dep-gate + spawn logic operates on the ids that actually landed on disk.
/// Bounded so a pathological store can't loop forever.
async fn persist_works_with_remint(
    store: &store::Store,
    mut works: Vec<domain::Work>,
) -> Result<(Vec<domain::Work>, Vec<domain::WorkId>), store::StoreError> {
    const MAX_REMINT_ATTEMPTS: usize = 5;
    for attempt in 0..MAX_REMINT_ATTEMPTS {
        match store.works().create_many(works.clone()).await {
            Ok(ids) => return Ok((works, ids)),
            Err(store::StoreError::AlreadyExists { id, .. }) => {
                warn!(
                    colliding_id = %id,
                    attempt = attempt + 1,
                    "create_many id collision; re-minting batch and retrying"
                );
                works = remint_work_batch(works);
            }
            Err(other) => return Err(other),
        }
    }
    Err(store::StoreError::AlreadyExists {
        collection: "works",
        id: format!("re-mint exhausted after {MAX_REMINT_ATTEMPTS} attempts"),
    })
}

/// Re-mint every `WorkId` in the batch and rewrite each Work's dependency
/// edges through the old->new remap. Deps referencing ids absent from the
/// batch are kept verbatim (defensive; the decomposer never emits them).
fn remint_work_batch(works: Vec<domain::Work>) -> Vec<domain::Work> {
    let remap: std::collections::HashMap<domain::WorkId, domain::WorkId> =
        works.iter().map(|w| (w.id.clone(), domain::WorkId::new())).collect();
    works
        .into_iter()
        .map(|mut w| {
            w.id = remap.get(&w.id).cloned().expect("every batch id is in the remap");
            w.dependencies = w
                .dependencies
                .iter()
                .map(|d| remap.get(d).cloned().unwrap_or_else(|| d.clone()))
                .collect();
            w
        })
        .collect()
}

/// Transition a Plan to `Stalled` (Reactor role) after a decompose or
/// Works-persist failure, so the operator sees a stuck Plan via
/// `loopr plans` / `loopr plan show` instead of a deceptive `Active` Plan
/// with zero Works and no Director. Best-effort: a failed transition is
/// logged, not propagated (the client already received its ACK). Recovery
/// is the operator's `loopr plan override <id> --to active` (re-decompose
/// of an emptied Plan is deferred-roadmap 2.2).
async fn stall_plan_after_decompose_failure<L>(ctx: &Arc<DaemonContext<L>>, plan: &mut domain::Plan)
where
    L: LlmClient + Send + Sync + 'static,
{
    if let Err(e) = crate::daemon::context::transition_and_persist_plan(
        &*ctx.summary_fanout,
        plan,
        Vec::new(),
        domain::PlanStatus::Stalled,
        domain::Role::Reactor,
        crate::daemon::context::PlanSummaryExtras::default(),
        false,
    )
    .await
    {
        warn!(plan_id = %plan.id, error = %e, "failed to transition Plan -> Stalled after decompose failure");
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

/// Handle `director.chat`: persist an operator message as an
/// `OperatorNote` routed to the named Plan's Director task. Phase 8
/// of `docs/design/2026-05-09-director-phase-2.md`.
///
/// Steps:
/// 1. Parse `plan_id` (PlanId::from_str is Infallible; string parse
///    just wraps it). Validate the Plan exists by fetching it from
///    the store; missing -> `RpcError::NotFound`.
/// 2. Truncate the message at
///    `DIRECTOR_CHAT_MESSAGE_BYTE_CAP` and append a marker so the
///    LLM sees a bounded payload (prompt-injection-by-volume bound).
/// 3. Construct an `OperatorNote` with the daemon's resolved `USER`
///    env var as author (falls back to `"operator"` if unset).
/// 4. Persist via `NotesStore::create`. Phase 9 will add the
///    `Notify::notify_one()` wakeup on the per-Plan `Arc<Notify>`;
///    this handler only persists so a daemon downtime cannot lose
///    the note.
#[instrument(
    name = "ipc.director_chat",
    level = "info",
    skip_all,
    fields(
        request_id = id,
        plan_id = %params.plan_id,
        message_bytes = params.message.len(),
        note_id = tracing::field::Empty,
    ),
)]
async fn handle_director_chat<L>(id: u64, params: DirectorChatParams, ctx: &Arc<DaemonContext<L>>) -> DaemonResponse
where
    L: LlmClient + Send + Sync + 'static,
{
    use std::str::FromStr;
    let plan_id = match domain::PlanId::from_str(&params.plan_id) {
        Ok(p) => p,
        Err(_) => {
            // PlanId::from_str is Infallible; this branch is
            // unreachable but the match is here for forward
            // compatibility if the typed-id parse ever gains
            // validation.
            return DaemonResponse::err(
                id,
                RpcError::InvalidParams(format!("plan_id parse failed: {}", params.plan_id)),
            );
        }
    };

    // Validate the Plan exists. The Plan record itself is not modified
    // by `director.chat`; this is a foreign-key check only.
    if let Err(e) = ctx.store.plans().get(&plan_id).await {
        warn!(request_id = id, plan_id = %plan_id, error = %e, "director.chat: plan not found");
        return DaemonResponse::err(id, map_store_error(e));
    }

    let message = truncate_chat_message(&params.message);
    let author = std::env::var("USER").unwrap_or_else(|_| "operator".to_string());
    let note = domain::OperatorNote::new(plan_id.clone(), author, message);
    let note_id = note.id.clone();
    tracing::Span::current().record("note_id", note_id.to_string().as_str());

    if let Err(e) = ctx.store.notes().create(note).await {
        warn!(request_id = id, error = %e, "director.chat: note create failed");
        return DaemonResponse::err(id, map_store_error(e));
    }

    info!(
        request_id = id,
        note_id = %note_id,
        "director.chat: note persisted"
    );

    // Phase 9: wake the Director task. The Notify is per-Plan; absence
    // from the map means either the Plan has no live Director (terminal
    // or pre-startup-reconcile) or the Director will pick up the note
    // on its next iteration via `list_unread_notes_for_plan`. Either
    // way, missing the wake-up is recoverable (latency-only).
    if let Some(notify) = ctx.operator_notifies.read().await.get(&plan_id) {
        notify.notify_one();
        debug!(request_id = id, plan_id = %plan_id, "director.chat: wakeup signalled");
    } else {
        debug!(
            request_id = id,
            plan_id = %plan_id,
            "director.chat: no live Director Notify; note will be picked up on next iteration"
        );
    }

    let result = DirectorChatResult {
        note_id: note_id.to_string(),
    };
    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(id, v),
        Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize director.chat: {e}"))),
    }
}

/// Handle `plan.override`. The operator nominates a target status; the
/// daemon runs `Plan::override_status(target, Role::Director)` and
/// persists the result. Today the only practical case is `Stalled ->
/// Active` (revive an escalated Plan). On the successful Stalled ->
/// Active transition, the daemon respawns a Director task for the
/// Plan so the operator does not need to restart the daemon to resume
/// supervision. Phase 10 of
/// `docs/design/2026-05-09-director-phase-2.md`.
#[instrument(
    name = "ipc.plan_override",
    level = "info",
    skip_all,
    fields(request_id = id, plan_id = %params.plan_id, target_status = %params.target_status),
)]
async fn handle_plan_override<L>(id: u64, params: PlanOverrideParams, ctx: &Arc<DaemonContext<L>>) -> DaemonResponse
where
    L: LlmClient + Send + Sync + 'static,
{
    use std::str::FromStr;
    let plan_id = match domain::PlanId::from_str(&params.plan_id) {
        Ok(p) => p,
        Err(_) => {
            return DaemonResponse::err(
                id,
                RpcError::InvalidParams(format!("plan_id parse failed: {}", params.plan_id)),
            );
        }
    };
    let target_status = match parse_plan_status(&params.target_status) {
        Ok(s) => s,
        Err(e) => {
            return DaemonResponse::err(id, RpcError::InvalidParams(e));
        }
    };

    let mut plan = match ctx.store.plans().get(&plan_id).await {
        Ok(p) => p,
        Err(e) => {
            warn!(request_id = id, plan_id = %plan_id, error = %e, "plan.override: plan not found");
            return DaemonResponse::err(id, map_store_error(e));
        }
    };
    let prior_status = plan.status;

    // F8: route the override through `transition_and_persist_plan` (the
    // SummaryFanout sink) like every other Plan writer, instead of a raw
    // `store.plans().update`. That gives OCC, the per-record summary
    // fanout, and the terminal-state event — all of which the raw write
    // bypassed. Children are fetched for the summary render (best-effort;
    // an empty list only degrades the summary, never blocks the override).
    let children = ctx.store.works().list_by_parent_id(&plan_id).await.unwrap_or_default();
    if let Err(e) = crate::daemon::context::transition_and_persist_plan(
        &*ctx.summary_fanout,
        &mut plan,
        children,
        target_status,
        domain::Role::Director,
        crate::daemon::context::PlanSummaryExtras::default(),
        true, // override
    )
    .await
    {
        warn!(
            request_id = id,
            plan_id = %plan_id,
            from = %prior_status,
            to = %target_status,
            error = %e,
            "plan.override: FSM/persist failed"
        );
        // FSM rejection and OCC `Stale` are both client-actionable
        // (bad target, or a racing writer already moved the Plan).
        return DaemonResponse::err(id, RpcError::InvalidRequest(format!("plan override failed: {e}")));
    }

    info!(
        request_id = id,
        plan_id = %plan_id,
        from = %prior_status,
        to = %target_status,
        "plan.override: persisted"
    );

    // Stalled -> Active means the operator revived an escalated Plan;
    // respawn the Director so supervision resumes without a daemon
    // restart. `spawn_director_for_plan` is a no-op (warn) during
    // shutdown, which is the only state where the respawn could race.
    if prior_status == domain::PlanStatus::Stalled && target_status == domain::PlanStatus::Active {
        spawn_director_for_plan(ctx, plan_id.clone()).await;
    }

    let result = PlanOverrideResult { plan };
    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(id, v),
        Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize plan.override: {e}"))),
    }
}

/// Handle `director.status`: look up the Plan, then read the per-Plan
/// snapshot the Director task wrote at the end of its last iteration.
/// `snapshot: None` is the "no live Director" wire form (Plan is
/// Stalled / Complete, or transient pre-spawn). Director Phase 2
/// follow-ups (Item 3) of
/// `docs/design/2026-05-12-director-phase-2-followups.md`.
#[instrument(
    name = "ipc.director_status",
    level = "debug",
    skip_all,
    fields(
        request_id = id,
        plan_id = %params.plan_id,
        plan_status = tracing::field::Empty,
        has_snapshot = tracing::field::Empty,
    ),
)]
async fn handle_director_status<L>(id: u64, params: DirectorStatusParams, ctx: &Arc<DaemonContext<L>>) -> DaemonResponse
where
    L: LlmClient + Send + Sync + 'static,
{
    use std::str::FromStr;
    let plan_id = match domain::PlanId::from_str(&params.plan_id) {
        Ok(p) => p,
        Err(_) => {
            return DaemonResponse::err(
                id,
                RpcError::InvalidParams(format!("plan_id parse failed: {}", params.plan_id)),
            );
        }
    };

    let plan = match ctx.store.plans().get(&plan_id).await {
        Ok(p) => p,
        Err(e) => {
            warn!(request_id = id, plan_id = %plan_id, error = %e, "director.status: plan not found");
            return DaemonResponse::err(id, map_store_error(e));
        }
    };
    tracing::Span::current().record("plan_status", plan.status.to_string().as_str());

    // Read the sidecar under a brief sync lock; clone the snapshot so
    // the lock drops before serde + response construction. Poison
    // degrades to "no snapshot," which the wire form represents as
    // `snapshot: None` — equivalent semantics to "no live Director."
    let snapshot_agents = ctx.director_statuses.read().ok().and_then(|m| m.get(&plan_id).cloned());
    tracing::Span::current().record("has_snapshot", snapshot_agents.is_some());

    let snapshot = snapshot_agents.map(|s| DirectorStatusSnapshot {
        mode: s.mode.as_str().to_string(),
        no_progress_streak: s.no_progress_streak,
        same_action_streak: s.same_action_streak,
        iteration: s.iteration,
        last_action_kind: s.last_action_kind,
        last_action_target_id: s.last_action_target_id,
        last_action_ts: s.last_action_ts,
        unread_note_count: s.unread_note_count,
        needs_operator_iters: s.needs_operator_iters,
    });

    let result = DirectorStatusResult {
        plan_id: plan_id.to_string(),
        plan_status: plan.status.to_string(),
        snapshot,
    };
    match serde_json::to_value(&result) {
        Ok(v) => DaemonResponse::ok(id, v),
        Err(e) => DaemonResponse::err(id, RpcError::Internal(format!("serialize director.status: {e}"))),
    }
}

/// Parse a lowercase Plan status string into the typed `PlanStatus`
/// enum. Mirrors the closed set in `domain::PlanStatus`.
fn parse_plan_status(s: &str) -> Result<domain::PlanStatus, String> {
    match s {
        "draft" => Ok(domain::PlanStatus::Draft),
        "active" => Ok(domain::PlanStatus::Active),
        "complete" => Ok(domain::PlanStatus::Complete),
        "stalled" => Ok(domain::PlanStatus::Stalled),
        other => Err(format!("unknown plan status: {other}")),
    }
}

/// Truncate the operator's message to `DIRECTOR_CHAT_MESSAGE_BYTE_CAP`
/// bytes, appending a marker so the LLM and downstream log readers
/// see that the payload was clipped. Truncation respects char
/// boundaries — the cut point retreats to the previous codepoint
/// boundary so the result is valid UTF-8 even when the byte cap falls
/// mid-codepoint.
fn truncate_chat_message(message: &str) -> String {
    if message.len() <= DIRECTOR_CHAT_MESSAGE_BYTE_CAP {
        return message.to_string();
    }
    let original_bytes = message.len();
    let mut cut = DIRECTOR_CHAT_MESSAGE_BYTE_CAP;
    while cut > 0 && !message.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 64);
    out.push_str(&message[..cut]);
    out.push_str(&format!("\n[truncated: original {original_bytes} bytes]"));
    out
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
        StoreError::Closed => RpcError::Internal("store closed (shutting down)".to_string()),
        StoreError::VersionMismatch { found, expected } => {
            RpcError::Internal(format!("store version mismatch: on-disk={found}, expected={expected}"))
        }
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
    let operator_notify = Arc::new(tokio::sync::Notify::new());
    ctx.operator_notifies
        .write()
        .await
        .insert(plan_id.clone(), Arc::clone(&operator_notify));
    let deps = DirectorDeps {
        llm: Arc::clone(&ctx.llm),
        store: Arc::clone(&ctx.store),
        context: Arc::clone(&ctx.context_builder),
        spawner: DaemonSpawner(Arc::clone(ctx)),
        config: ctx.director_config.clone(),
        shutdown: Arc::clone(&ctx.shutdown_notify),
        operator_notify,
        director_statuses: Arc::clone(&ctx.director_statuses),
    };
    let mut directors = ctx.director_tasks.lock().await;
    let plan_id_for_log = plan_id.clone();
    let operator_notifies = Arc::clone(&ctx.operator_notifies);
    let director_statuses = Arc::clone(&ctx.director_statuses);
    let plan_id_for_cleanup = plan_id.clone();
    // Capture the exact Notify this task inserted so the exit cleanup can
    // compare-before-remove: a Stalled -> Active override may respawn a
    // fresh Director (inserting a NEW Notify) before this task's cleanup
    // runs; an unconditional remove would delete the respawn's Notify.
    let notify_for_cleanup = Arc::clone(&deps.operator_notify);
    directors.spawn(async move {
        // Panic posture: `catch_unwind` so a panic inside `run_director`
        // is logged and the per-Plan Notify + status-snapshot cleanup
        // below still runs (the JoinSet would otherwise swallow the
        // panic and leak both sidecar entries).
        let result = std::panic::AssertUnwindSafe(run_director(&plan_id, &deps))
            .catch_unwind()
            .await;
        match result {
            Ok(Ok(())) => info!(plan_id = %plan_id_for_log, "director task exited Ok"),
            Ok(Err(DirectorError::NeedHelp(reason))) => warn!(
                plan_id = %plan_id_for_log,
                reason = %reason,
                "director exited with NeedHelp"
            ),
            Ok(Err(e)) => tracing::error!(plan_id = %plan_id_for_log, error = %e, "director exited with error"),
            Err(panic) => {
                let msg = crate::daemon::context::panic_message(&*panic);
                tracing::error!(plan_id = %plan_id_for_log, panic = %msg, "director task panicked");
            }
        }
        // Phase 9: drop the per-Plan operator Notify on Director task
        // exit, but ONLY if the map still holds the Notify THIS task
        // inserted (compare-before-remove) — a respawned Director may have
        // replaced it. See startup_reconcile_directors for the rationale.
        {
            let mut map = operator_notifies.write().await;
            if map.get(&plan_id_for_cleanup).is_some_and(|n| Arc::ptr_eq(n, &notify_for_cleanup)) {
                map.remove(&plan_id_for_cleanup);
            }
        }
        // Director Phase 2 follow-ups (Item 3): drop the per-Plan
        // status snapshot on task exit so a subsequent
        // `director.status` IPC call returns `snapshot: None` (the
        // "not running" wire form) instead of stale data.
        if let Ok(mut m) = director_statuses.write() {
            m.remove(&plan_id_for_cleanup);
        }
    });
}

#[cfg(test)]
mod tests;
