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
use log::{debug, info, warn};
use tokio::sync::broadcast;

use crate::agents::{AgentSession, AgentStatus, AgentType};
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

/// Run a single Work item through the full Implementer lifecycle.
///
/// This is the entry point for pull-based workers. It:
/// 1. Transitions Work Ready -> InProgress (returns Ok if already claimed)
/// 2. Creates an AgentSession
/// 3. Delegates to `run_agent_task` for the full lifecycle (worktree, LLM, agent loop, handback, cleanup)
///
/// Race handling: if the Ready -> InProgress transition fails (another worker grabbed it),
/// returns Ok(()) immediately - this is expected contention, not an error.
pub async fn run_single_work(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    implementer_config: &crate::config::AgentRoleConfig,
    work_id: &str,
    worker_id: u32,
) -> Result<()> {
    info!("Worker {} attempting Work {}", worker_id, work_id);

    // Step 1: Transition Work Ready -> InProgress.
    // Use the bridge to go through the handler (FSM validation + persistence).
    let bridge = crate::agents::bridge::AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr.clone(),
        stores.config.clone(),
    );
    let transition_resp = bridge.request(
        "work.transition",
        serde_json::json!({
            "id": work_id,
            "target_status": "InProgress",
            "role": "coordinator",
            "assignee": "implementer",
        }),
    );
    if transition_resp.is_error() {
        // Expected contention: another worker grabbed this Work first.
        info!(
            "Worker {} could not claim Work {} (likely already claimed): {:?}",
            worker_id,
            work_id,
            transition_resp.error.as_ref().map(|e| &e.message)
        );
        return Ok(());
    }

    // Step 2: Check pool capacity and dedup before creating session
    {
        let sessions = stores.read_agent_sessions()?;
        let active_count = sessions
            .values()
            .filter(|s| s.agent_type == AgentType::Implementer && !s.status.is_terminal())
            .count();
        let max_pool = stores.config.agents.implementer.max_pool as usize;
        if active_count >= max_pool {
            warn!(
                "Worker {} pool exhausted ({}/{}), skipping Work {}",
                worker_id, active_count, max_pool, work_id
            );
            return Ok(());
        }

        // Dedup: if an implementer is already running on this work, skip
        let has_existing = sessions.values().any(|s| {
            s.agent_type == AgentType::Implementer && !s.status.is_terminal() && s.work_id.as_deref() == Some(work_id)
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
    let mut session = AgentSession::new(AgentType::Implementer, implementer_config.model.clone());
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

    // Step 4: Run the full agent lifecycle (worktree, LLM, loop, handback, cleanup)
    run_agent_task(
        session_id,
        AgentType::Implementer,
        stores.clone(),
        event_tx.clone(),
        worktree_mgr.clone(),
    )
    .await;

    Ok(())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
