mod action;
pub(crate) mod context;
mod lifecycle;
mod llm;
pub mod result;
mod util;

// Re-exports: preserve the public API from the old single-file executor.rs
pub use action::execute_action;
pub use lifecycle::run_agent_task;
pub use result::ActionResult;
pub use util::resolve_worktree_base;

use std::sync::Arc;

use eyre::{Result, eyre};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::agents::{AgentKind, AgentSession, AgentStatus};
use crate::daemon::context::Stores;
use crate::domain::work::WorkStatus;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

/// Pre-flight acceptance-criteria check.
///
/// Disabled in Phase 1: the `files` field was removed from Work (reactive conflict model).
/// File paths are no longer available at planning time, so we cannot read files to check AC.
/// TODO: Phase 3 - re-enable by deriving file paths from worktree `paths`.
///
/// Returns `None` (fall through to implementer) unconditionally.
async fn preflight_ac_check(_stores: &Arc<Stores>, _work_id: &str) -> Option<bool> {
    None
}

/// Run a single Work item through the full Implementer lifecycle.
///
/// This is the entry point for pull-based workers. The Work must already be
/// InProgress (claimed atomically by `next_assignable_work`). It:
/// 1. Verifies Work is still InProgress
/// 2. Creates an AgentSession
/// 3. Delegates to `run_agent_task` for the full lifecycle (worktree, LLM, agent loop, handback, cleanup)
pub async fn run_single_work(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    implementer_config: &crate::config::AgentRoleConfig,
    work_id: &str,
    worker_id: u32,
) -> Result<()> {
    info!("Worker {} attempting Work {}", worker_id, work_id);

    let bridge = crate::agents::bridge::AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr.clone(),
        stores.config.clone(),
    );

    // Pre-flight acceptance-criteria check (Fix 6).
    // If the current repo already satisfies the work's AC, short-circuit to Done.
    if let Some(true) = preflight_ac_check(stores, work_id).await {
        info!(
            "Worker {} pre-flight PASS: Work {} AC already satisfied, marking Done",
            worker_id, work_id
        );
        let done_resp = bridge.request(
            "work.transition",
            serde_json::json!({
                "id": work_id,
                "target_status": "Done",
                "role": "coordinator",
            }),
        );
        if done_resp.is_error() {
            info!(
                "Worker {} pre-flight Done transition failed (contention): {:?}",
                worker_id,
                done_resp.error.as_ref().map(|e| &e.message)
            );
        }
        return Ok(());
    }

    // Step 1: Work is already InProgress (claimed atomically by next_assignable_work).
    // Verify it's still InProgress before proceeding - another reconciliation pass
    // could have changed it.
    {
        let works = stores.read_works()?;
        match works.get(work_id) {
            Some(w) if w.status() == WorkStatus::InProgress => {}
            Some(w) => {
                info!(
                    "Worker {} skipping Work {} - status is {:?}, expected InProgress",
                    worker_id,
                    work_id,
                    w.status()
                );
                return Ok(());
            }
            None => {
                info!("Worker {} skipping Work {} - not found in store", worker_id, work_id);
                return Ok(());
            }
        }
    }

    // Step 2: Check pool capacity and dedup before creating session
    {
        let sessions = stores.read_agent_sessions()?;
        let active_count = sessions
            .values()
            .filter(|s| s.agent_type == AgentKind::Implementer && !s.status().is_terminal())
            .count();
        let effective_max = if implementer_config.max_pool == crate::config::MAX_POOL_UNLIMITED {
            stores.config.agents.worker_pool_size.resolve() as usize
        } else {
            implementer_config.max_pool as usize
        };
        if active_count >= effective_max {
            warn!(
                "Worker {} pool exhausted ({}/{}), skipping Work {}",
                worker_id, active_count, effective_max, work_id
            );
            return Ok(());
        }

        // Dedup: if an implementer is already running on this work, skip
        let has_existing = sessions.values().any(|s| {
            s.agent_type == AgentKind::Implementer && !s.status().is_terminal() && s.work_id.as_deref() == Some(work_id)
        });
        if has_existing {
            info!(
                "Worker {} skipping Work {} - implementer already running",
                worker_id, work_id
            );
            return Ok(());
        }
    }

    // Step 3: Create AgentSession
    let mut session = AgentSession::new(AgentKind::Implementer, implementer_config.model.clone());
    session.work_id = Some(work_id.to_string());
    let session_id = session.id.clone();

    // Persist session
    if let Some(store) = &stores.store
        && let Ok(mut s) = store.lock().map_err(|_| eyre!("store lock poisoned"))
        && let Err(e) = s.create(session.clone())
    {
        warn!("Worker {} failed to persist session: {}", worker_id, e);
        return Err(eyre!("failed to create agent session: {}", e));
    }
    stores.write_agent_sessions()?.insert(session_id.clone(), session);
    let _ = event_tx.send(DaemonEvent::record_created("agent_session", &session_id));
    debug!("[agent_status] {}: -> Starting (worker spawn)", session_id);
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Starting));

    info!(
        "Worker {} created session {} for Work {}",
        worker_id, session_id, work_id
    );

    // Step 4: Spawn the full agent lifecycle as a Tokio task and register its handle.
    //
    // Must use tokio::spawn (not .await) so the JoinHandle can be stored in
    // agent_handles before the reconciler next fires. Without a registered handle,
    // reconcile() cannot distinguish a live worker-spawned session from an orphaned
    // one, and resets InProgress work to Blocked every 30s while the implementer runs.
    let task_stores = stores.clone();
    let task_event_tx = event_tx.clone();
    let task_worktree_mgr = worktree_mgr.clone();
    let task_session_id = session_id.clone();
    let handle = tokio::spawn(async move {
        run_agent_task(
            task_session_id,
            AgentKind::Implementer,
            task_stores,
            task_event_tx,
            task_worktree_mgr,
        )
        .await;
    });

    // Register handle so reconciler sees this session as live.
    // Lock ordering: agent_handles is always acquired after agent_sessions.
    stores.lock_agent_handles()?.insert(session_id.clone(), handle);

    Ok(())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
