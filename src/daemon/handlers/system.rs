use std::sync::Arc;
use std::sync::atomic::Ordering;

use eyre::eyre;
use serde_json::json;
use tokio::sync::broadcast;
use tracing::debug;

use crate::agents::AgentSession;
use crate::domain::bundle::{Bundle, BundleStatus};
use crate::domain::coordinator_goal::CoordinatorGoal;
use crate::domain::doc::Doc;
use crate::domain::learning::Learning;
use crate::domain::lock::Lock;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::spec::Spec;
use crate::domain::tick::{Tick, TickStatus};
use crate::domain::work::{Work, WorkStatus};
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::Record;

use crate::daemon::context::Stores;

pub(super) fn handle_handshake(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_handshake()");
        let server_version = crate::version();
        let client_version = req
            .params
            .get("client_version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let version_match = client_version == server_version;
        if !version_match {
            tracing::warn!(
                "Client version mismatch: client={}, server={}",
                client_version,
                server_version
            );
        }

        Ok(DaemonResponse::ok(
            req.id,
            json!({
                "server_version": server_version,
                "client_version": client_version,
                "version_match": version_match,
                "protocol": "ndjson/1",
                "session_id": stores.session_id,
            }),
        ))
    })
}

pub(super) fn handle_system_init(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_system_init()");
        let store_arc = match &stores.store {
            Some(s) => s,
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::internal("TaskStore not initialized"),
                ));
            }
        };

        // Install git merge driver and .gitattributes (best-effort)
        let git_hooks_ok = {
            let store = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))?;
            match store.install_git_hooks() {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("Failed to install git hooks (non-fatal): {}", e);
                    false
                }
            }
        };

        // Return the list of collection names
        let collections = vec![
            Plan::collection_name(),
            Spec::collection_name(),
            Phase::collection_name(),
            Work::collection_name(),
            Doc::collection_name(),
            Bundle::collection_name(),
            Tick::collection_name(),
            Learning::collection_name(),
            Lock::collection_name(),
            CoordinatorGoal::collection_name(),
            AgentSession::collection_name(),
        ];

        Ok(DaemonResponse::ok(
            req.id,
            json!({ "collections": collections, "git_hooks_installed": git_hooks_ok }),
        ))
    })
}

pub(super) fn handle_status(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_status()");
        let plans = stores.read_plans()?.len();
        let specs = stores.read_specs()?.len();
        let phases = stores.read_phases()?.len();
        let works = stores.read_works()?.len();
        let bundles = stores.read_bundles()?.len();
        let ticks = stores.read_ticks()?.len();
        let learnings = stores.read_learnings()?.len();
        let locks = stores.read_locks()?.len();
        let agent_sessions = stores.read_agent_sessions()?.len();

        // TaskStore stats (when available)
        let taskstore_stats = if let Some(store) = &stores.store {
            let s = store.lock().map_err(|_| eyre!("taskstore lock poisoned"))?;
            let ts_plans = s.list::<Plan>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_specs = s.list::<Spec>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_phases = s.list::<Phase>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_works = s.list::<Work>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_bundles = s.list::<Bundle>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_ticks = s.list::<Tick>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_learnings = s.list::<Learning>(&[]).map(|v| v.len()).unwrap_or(0);
            let ts_locks = s.list::<Lock>(&[]).map(|v| v.len()).unwrap_or(0);
            json!({
                "enabled": true,
                "counts": {
                    "plans": ts_plans,
                    "specs": ts_specs,
                    "phases": ts_phases,
                    "works": ts_works,
                    "bundles": ts_bundles,
                    "ticks": ts_ticks,
                    "learnings": ts_learnings,
                    "locks": ts_locks,
                }
            })
        } else {
            json!({ "enabled": false })
        };

        // Gap #33: Current Tick SHA - find the latest Published tick
        let current_tick_sha: Option<String> = {
            let ticks_map = stores.read_ticks()?;
            ticks_map
                .values()
                .filter(|t| t.status() == TickStatus::Published)
                .max_by_key(|t| t.number)
                .and_then(|t| t.integration_sha.clone())
        };

        // Gap #33: Latest published tick ID for staleness check
        let latest_tick_id: Option<String> = {
            let ticks_map = stores.read_ticks()?;
            ticks_map
                .values()
                .filter(|t| t.status() == TickStatus::Published)
                .max_by_key(|t| t.number)
                .map(|t| t.id.clone())
        };

        // Gap #33: Stale works count
        let stale_works: usize = {
            let wis = stores.read_works()?;
            let bundles_map = stores.read_bundles()?;
            if let Some(ref latest_tid) = latest_tick_id {
                wis.values()
                    .filter(|wi| wi.status() == WorkStatus::InProgress)
                    .filter(|wi| {
                        bundles_map.values().any(|b| {
                            b.work_id == wi.id
                                && !matches!(
                                    b.status(),
                                    BundleStatus::Merged | BundleStatus::Rejected | BundleStatus::Superseded
                                )
                                && b.base_tick_id.as_ref().is_some_and(|btid| btid != latest_tid)
                        })
                    })
                    .count()
            } else {
                0
            }
        };

        // Reconciliation health (populated by run_reconciler once it runs)
        let reconciliation = json!({
            "last_sweep_at": stores.reconciliation_last_sweep_at.load(Ordering::Relaxed),
            "checked": stores.reconciliation_checked.load(Ordering::Relaxed),
            "fixed": stores.reconciliation_fixed.load(Ordering::Relaxed),
            "catastrophic": stores.reconciliation_catastrophic.load(Ordering::Relaxed),
            "degraded": stores.degraded.load(Ordering::Relaxed),
        });

        Ok(DaemonResponse::ok(
            req.id,
            json!({
                "version": crate::version(),
                "pid": std::process::id(),
                "counts": {
                    "plans": plans,
                    "specs": specs,
                    "phases": phases,
                    "works": works,
                    "bundles": bundles,
                    "ticks": ticks,
                    "learnings": learnings,
                    "locks": locks,
                    "agent_sessions": agent_sessions,
                },
                "taskstore": taskstore_stats,
                "current_tick_sha": current_tick_sha,
                "stale_works": stale_works,
                "session_id": stores.session_dir.as_ref().and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string())),
                "reconciliation": reconciliation,
            }),
        ))
    })
}

pub(super) fn handle_shutdown(event_tx: &broadcast::Sender<DaemonEvent>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_shutdown()");
        // Broadcast a shutdown event so the accept loop can pick it up
        let _ = event_tx.send(DaemonEvent::new("system.shutdown", json!({})));
        Ok(DaemonResponse::ok(req.id, json!({ "status": "shutting_down" })))
    })
}

pub(super) fn handle_recover(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_recover()");
        let was_degraded = stores.degraded.swap(false, Ordering::Relaxed);
        Ok(DaemonResponse::ok(
            req.id,
            json!({ "was_degraded": was_degraded, "degraded": false }),
        ))
    })
}
