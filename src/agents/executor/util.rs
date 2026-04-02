use log::{debug, error, warn};

use crate::agents::agent_logger::AgentLogger;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::{AgentSession, AgentType};
use crate::daemon::context::Stores;
use crate::domain::bundle::BundleStatus;
use crate::domain::tick::TickStatus;

/// Normalize a collection name from plural to singular for IPC method dispatch.
/// The LLM may emit "plans", "specs", "phases", "works", "bundles", "ticks"
/// but IPC methods use singular: "plan", "spec", "phase", "work", "bundle", "tick".
pub(super) fn normalize_collection(collection: &str) -> &str {
    debug!("normalize_collection(collection={})", collection);
    match collection {
        "plans" => "plan",
        "specs" => "spec",
        "phases" => "phase",
        "works" => "work",
        "bundles" => "bundle",
        "ticks" => "tick",
        other => other,
    }
}

/// Returns the integration SHA of the latest Published Tick,
/// or "HEAD" if no Ticks have been published yet.
pub fn resolve_worktree_base(stores: &Stores) -> String {
    debug!("resolve_worktree_base()");
    let Ok(ticks) = stores.read_ticks() else {
        error!("ticks lock poisoned");
        return "HEAD".to_string();
    };
    ticks
        .values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .and_then(|t| t.integration_sha.clone())
        .unwrap_or_else(|| "HEAD".to_string())
}

/// Resolve the ID of the latest Published Tick from stores.
/// Returns None if no Published Tick exists (bootstrap case).
pub(super) fn resolve_latest_published_tick_id(stores: &Stores) -> Option<String> {
    debug!("resolve_latest_published_tick_id()");
    let ticks = stores.read_ticks().ok()?;
    ticks
        .values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .map(|t| t.id.clone())
}

/// Determine the Work transition after an Implementer's agent loop exits.
///
/// Returns `Some(target_status)` to transition, or `None` to skip (sibling still active).
///
/// Decision table:
/// | Sibling active? | Bundle state         | Work transition |
/// |---|---|---|
/// | Yes             | -                    | (skip)          |
/// | No              | active Bundle exists | "InReview"      |
/// | No              | all Rejected/none    | "Blocked"       |
pub(super) fn determine_work_handback(
    stores: &Stores,
    work_id: &str,
    session_id: &str,
    _succeeded: bool,
) -> Option<&'static str> {
    // If a sibling implementer is still active, don't touch the Work.
    let sessions = stores.read_agent_sessions().ok()?;
    let sibling_active = sessions.values().any(|s| {
        s.id != session_id
            && s.agent_type == AgentType::Implementer
            && s.work_id.as_deref() == Some(work_id)
            && !s.status.is_terminal()
    });
    drop(sessions);

    if sibling_active {
        return None; // let the sibling finish
    }

    // Did the agent produce a usable Bundle?
    let bundles = stores.read_bundles().ok()?;
    let has_active_bundle = bundles
        .values()
        .any(|b| b.work_id == work_id && !matches!(b.status, BundleStatus::Rejected | BundleStatus::Superseded));

    Some(if has_active_bundle { "InReview" } else { "Blocked" })
}

/// Auto-acquire an advisory lock before a file write/edit. Returns the lock ID if a new
/// lock was created, or None if the holder already holds a lock on this resource.
pub(super) fn auto_acquire_write_lock(bridge: &AgentIpcBridge, resource: &str, holder_id: &str) -> Option<String> {
    // Check if we already hold a lock on this resource
    let check = bridge.request(
        "lock.list",
        serde_json::json!({ "resource": resource, "holder_id": holder_id, "active_only": true }),
    );
    let already_held = check
        .result
        .as_ref()
        .and_then(|v| v.as_array())
        .is_some_and(|locks| !locks.is_empty());

    if already_held {
        return None; // Already locked by us
    }

    // Check if another agent already holds a lock (advisory warning)
    let existing = bridge.request(
        "lock.list",
        serde_json::json!({ "resource": resource, "active_only": true }),
    );
    if let Some(locks) = existing.result.as_ref().and_then(|v| v.as_array()) {
        for lock in locks {
            if let Some(other) = lock.get("holder_id").and_then(|v| v.as_str())
                && other != holder_id
            {
                log::warn!(
                    "advisory lock contention: {} already holds lock on {}, acquiring concurrent lock for {}",
                    other,
                    resource,
                    holder_id
                );
            }
        }
    }

    // Acquire new lock
    let resp = bridge.request(
        "lock.create",
        serde_json::json!({
            "resource": resource,
            "holder_id": holder_id,
            "granted_by": holder_id,
        }),
    );
    resp.result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|id| id.as_str())
        .map(String::from)
}

/// Release all advisory locks held by a given holder (work ID). Called at agent exit.
pub(super) fn release_agent_locks(bridge: &AgentIpcBridge, holder_id: &str, agent_log: &AgentLogger) {
    let resp = bridge.request(
        "lock.list",
        serde_json::json!({ "holder_id": holder_id, "active_only": true }),
    );
    if let Some(locks) = resp.result.as_ref().and_then(|v| v.as_array()) {
        for lock in locks {
            if let Some(lock_id) = lock.get("id").and_then(|v| v.as_str()) {
                let _ = bridge.request("lock.release", serde_json::json!({ "id": lock_id }));
            }
        }
        if !locks.is_empty() {
            agent_log.info(&format!("released {} advisory lock(s)", locks.len()));
        }
    }
}

/// Persist an agent session to the TaskStore backend.
pub(super) fn persist_session(stores: &Stores, session: &AgentSession) {
    debug!("persist_session(session_id={})", session.id);
    if let Some(store) = &stores.store
        && let Ok(mut s) = store.lock().map_err(|_| eyre::eyre!("store lock poisoned"))
        && let Err(e) = s.update(session.clone())
    {
        warn!("Failed to persist agent session {} to TaskStore: {}", session.id, e);
    }
}
