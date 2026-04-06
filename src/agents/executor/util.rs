use tracing::{debug, error, info, warn};

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::{AgentKind, AgentSession};
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
        .filter(|t| t.status() == TickStatus::Published)
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
        .filter(|t| t.status() == TickStatus::Published)
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
            && s.agent_type == AgentKind::Implementer
            && s.work_id.as_deref() == Some(work_id)
            && !s.status().is_terminal()
    });
    drop(sessions);

    if sibling_active {
        return None; // let the sibling finish
    }

    // Did the agent produce a usable Bundle?
    let bundles = stores.read_bundles().ok()?;
    let has_active_bundle = bundles
        .values()
        .any(|b| b.work_id == work_id && !matches!(b.status(), BundleStatus::Rejected | BundleStatus::Superseded));

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
                tracing::warn!(
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
pub(super) fn release_agent_locks(bridge: &AgentIpcBridge, holder_id: &str, prefix: &str) {
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
            info!("{} released {} advisory lock(s)", prefix, locks.len());
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::executor::tests::test_stores;
    use crate::agents::{AgentKind, AgentSession, AgentStatus};
    use crate::domain::bundle::BundleStatus;
    use crate::domain::tick::TickStatus;
    use crate::test_util::TestDir;
    use crate::worktree::manager::WorktreeManager;

    use super::{
        determine_work_handback, release_agent_locks, resolve_latest_published_tick_id, resolve_worktree_base,
    };
    use tokio::sync::broadcast;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_resolve_worktree_base_no_ticks() {
        let dir = TestDir::new("loopr-wt-base-none");
        let stores = test_stores(&dir);
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "HEAD");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_resolve_worktree_base_no_published_ticks() {
        let dir = TestDir::new("loopr-wt-base-nopub");
        let stores = test_stores(&dir);
        {
            let mut ticks = stores.ticks.write().unwrap();
            let mut t = crate::domain::tick::Tick::new(1);
            t.integration_sha = Some("abc123".to_string());
            ticks.insert(t.id.clone(), t);
        }
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "HEAD");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_resolve_worktree_base_picks_latest_published() {
        let dir = TestDir::new("loopr-wt-base-latest");
        let stores = test_stores(&dir);
        {
            let mut ticks = stores.ticks.write().unwrap();

            let mut t1 = crate::domain::tick::Tick::new(1);
            t1.force_status(crate::domain::tick::TickStatus::Published);
            t1.integration_sha = Some("sha_tick_1".to_string());
            ticks.insert(t1.id.clone(), t1);

            let mut t2 = crate::domain::tick::Tick::new(3);
            t2.force_status(crate::domain::tick::TickStatus::Published);
            t2.integration_sha = Some("sha_tick_3".to_string());
            ticks.insert(t2.id.clone(), t2);

            let mut t3 = crate::domain::tick::Tick::new(2);
            t3.force_status(crate::domain::tick::TickStatus::Published);
            t3.integration_sha = Some("sha_tick_2".to_string());
            ticks.insert(t3.id.clone(), t3);
        }
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "sha_tick_3");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_resolve_worktree_base_published_without_sha_falls_back() {
        let dir = TestDir::new("loopr-wt-base-nosha");
        let stores = test_stores(&dir);
        {
            let mut ticks = stores.ticks.write().unwrap();
            let mut t = crate::domain::tick::Tick::new(1);
            t.force_status(crate::domain::tick::TickStatus::Published);
            t.integration_sha = None;
            ticks.insert(t.id.clone(), t);
        }
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "HEAD");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_resolve_latest_published_tick_id_none_at_bootstrap() {
        let dir = TestDir::new("loopr-exec-tickid-none");
        let stores = test_stores(&dir);
        let result = resolve_latest_published_tick_id(&stores);
        assert!(result.is_none(), "expected None at bootstrap, got: {:?}", result);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_resolve_latest_published_tick_id_returns_published() {
        let dir = TestDir::new("loopr-exec-tickid-pub");
        let stores = test_stores(&dir);

        let mut tick = crate::domain::tick::Tick::new(1);
        tick.force_status(TickStatus::Published);
        tick.integration_sha = Some("abc123".to_string());
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let result = resolve_latest_published_tick_id(&stores);
        assert_eq!(result, Some(tick_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_handback_succeeded_no_bundles_returns_blocked() {
        let dir = TestDir::new("loopr-handback-ok");
        let stores = test_stores(&dir);
        let result = determine_work_handback(&stores, "wi-1", "sess-1", true);
        assert_eq!(result, Some("Blocked"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_handback_failed_no_bundles_returns_blocked() {
        let dir = TestDir::new("loopr-handback-nobd");
        let stores = test_stores(&dir);
        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("Blocked"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_handback_failed_with_active_bundle_returns_in_review() {
        let dir = TestDir::new("loopr-handback-actbd");
        let stores = test_stores(&dir);

        let mut bundle = crate::domain::bundle::Bundle::new(
            "wi-1".to_string(),
            None,
            "agent/wi-1".to_string(),
            vec!["claim".into()],
        );
        bundle.force_status(BundleStatus::Accepted);
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("InReview"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_handback_failed_all_rejected_bundles_returns_blocked() {
        let dir = TestDir::new("loopr-handback-rejbd");
        let stores = test_stores(&dir);

        let mut bundle = crate::domain::bundle::Bundle::new(
            "wi-1".to_string(),
            None,
            "agent/wi-1".to_string(),
            vec!["claim".into()],
        );
        bundle.force_status(BundleStatus::Rejected);
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("Blocked"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_handback_failed_sibling_active_returns_none() {
        let dir = TestDir::new("loopr-handback-sib");
        let stores = test_stores(&dir);

        let mut sibling = AgentSession::new(AgentKind::Implementer, "test-model".into());
        sibling.work_id = Some("wi-1".to_string());
        sibling.transition_to(AgentStatus::Running).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(sibling.id.clone(), sibling);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_handback_failed_sibling_terminal_checks_bundles() {
        let dir = TestDir::new("loopr-handback-sibterm");
        let stores = test_stores(&dir);

        let mut sibling = AgentSession::new(AgentKind::Implementer, "test-model".into());
        sibling.work_id = Some("wi-1".to_string());
        sibling.transition_to(AgentStatus::Running).unwrap();
        sibling.transition_to(AgentStatus::Completed).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(sibling.id.clone(), sibling);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("Blocked"));
    }

    // --- ReadFile dedup tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_release_agent_locks_cleans_up() {
        let dir = TestDir::new("loopr-exec-rellock-cleanup");
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());
        bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/a.rs", "holder_id": "wi-rel", "granted_by": "wi-rel" }),
        );
        bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/b.rs", "holder_id": "wi-rel", "granted_by": "wi-rel" }),
        );

        let check = bridge.request(
            "lock.list",
            serde_json::json!({ "holder_id": "wi-rel", "active_only": true }),
        );
        assert_eq!(check.result.as_ref().unwrap().as_array().unwrap().len(), 2);

        release_agent_locks(&bridge, "wi-rel", "[test:test-session]");

        let after = bridge.request(
            "lock.list",
            serde_json::json!({ "holder_id": "wi-rel", "active_only": true }),
        );
        let remaining = after.result.as_ref().unwrap().as_array().unwrap();
        assert!(
            remaining.is_empty(),
            "expected 0 active locks after release, got {}",
            remaining.len()
        );
    }
}
