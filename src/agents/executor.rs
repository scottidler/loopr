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

use crate::agents::{AgentKind, AgentSession, AgentStatus};
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

/// Pre-flight acceptance-criteria check.
///
/// Reads the files listed in `work.resource_tags`, presents their contents
/// alongside `work.acceptance_criteria`, and asks haiku whether the current
/// code already satisfies all of them.
///
/// Returns `Some(true)` if AC are satisfied, `Some(false)` if not, and `None`
/// if the check cannot run (empty resource_tags, empty AC, missing files, or
/// API error). The caller treats `None` as "fall through to implementer".
async fn preflight_ac_check(stores: &Arc<Stores>, work_id: &str) -> Option<bool> {
    debug!("preflight_ac_check(work_id={})", work_id);

    let (resource_tags, acceptance_criteria) = {
        let works = stores.read_works().ok()?;
        let work = works.get(work_id)?;
        if work.resource_tags.is_empty() || work.acceptance_criteria.is_empty() {
            debug!("preflight_ac_check: skipping {} (no resource_tags or AC)", work_id);
            return None;
        }
        (work.resource_tags.clone(), work.acceptance_criteria.clone())
    };

    let repo_path = stores.config.project.repo_path.clone();
    let mut file_contents: Vec<(String, String)> = Vec::new();
    for tag in &resource_tags {
        let path = repo_path.join(tag.trim_start_matches("./"));
        match std::fs::read_to_string(&path) {
            Ok(content) => file_contents.push((tag.clone(), content)),
            Err(e) => debug!("preflight_ac_check: could not read {}: {}", tag, e),
        }
    }
    if file_contents.is_empty() {
        debug!("preflight_ac_check: skipping {} (no files readable)", work_id);
        return None;
    }

    let api_key_env = &stores.config.agents.implementer.api_key_env;
    let api_key = std::env::var(api_key_env).ok()?;

    let mut prompt = String::from(
        "Do the following files already satisfy ALL of the acceptance criteria listed below?\n\n\
         Answer with exactly one word: YES or NO.\n\n\
         ## Acceptance Criteria\n\n",
    );
    for ac in &acceptance_criteria {
        prompt.push_str(&format!("- {}\n", ac));
    }
    prompt.push_str("\n## Current File Contents\n");
    for (path, content) in &file_contents {
        let truncated = &content[..content.len().min(4000)];
        prompt.push_str(&format!("\n### `{}`\n```\n{}\n```\n", path, truncated));
    }

    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": 16,
        "temperature": 0.0,
        "messages": [{"role": "user", "content": prompt}],
    });

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .timeout(std::time::Duration::from_secs(30))
        .json(&body)
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        warn!("preflight_ac_check: API returned {}", response.status());
        return None;
    }

    let resp_body: serde_json::Value = response.json().await.ok()?;
    let text = resp_body
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|block| block.get("text"))
        .and_then(|t| t.as_str())?
        .trim()
        .to_uppercase();

    info!("preflight_ac_check(work_id={}): response={:?}", work_id, text);
    Some(text.starts_with("YES"))
}

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

    // Step 1: Transition Work Ready -> InProgress.
    // Use the bridge to go through the handler (FSM validation + persistence).
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
            .filter(|s| s.agent_type == AgentKind::Implementer && !s.status().is_terminal())
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

    // Step 4: Run the full agent lifecycle (worktree, LLM, loop, handback, cleanup)
    run_agent_task(
        session_id,
        AgentKind::Implementer,
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
