//! Deterministic Integrator task — no LLM.
//!
//! The Integrator automates the Tick lifecycle: find Accepted Bundles, create a Tick,
//! seal it, run validation commands, then publish or fail. Every decision is an
//! if/then/else on data from the stores — no prompts, no parsing, no temperature.

use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use log::{error, info, warn};
use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::{AgentSession, AgentStatus};
use crate::config::IntegratorConfig;
use crate::daemon::context::Stores;
use crate::domain::bundle::BundleStatus;
use crate::domain::tick::TickStatus;
use crate::ipc::protocol::DaemonEvent;

/// Check if the integrator session has been cancelled.
fn is_session_cancelled(stores: &Stores, session_id: &str) -> bool {
    let sessions = stores.agent_sessions.read().unwrap();
    sessions
        .get(session_id)
        .map(|s| s.status == AgentStatus::Cancelled)
        .unwrap_or(true)
}

/// Find the latest published Tick number from the stores.
fn latest_published_tick_id(stores: &Stores) -> Option<String> {
    let ticks = stores.ticks.read().unwrap();
    ticks
        .values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .map(|t| t.id.clone())
}

/// Get the next tick number (max existing + 1, or 1 if none).
fn next_tick_number(stores: &Stores) -> u32 {
    let ticks = stores.ticks.read().unwrap();
    ticks.values().map(|t| t.number).max().unwrap_or(0) + 1
}

/// Check if there's already a Tick in a non-terminal state (Open, Sealing, Validating).
fn has_tick_in_progress(stores: &Stores) -> bool {
    let ticks = stores.ticks.read().unwrap();
    ticks.values().any(|t| {
        matches!(
            t.status,
            TickStatus::Open | TickStatus::Sealing | TickStatus::Validating
        )
    })
}

/// Recover stuck Ticks (Sealing/Validating) from a previous crash.
/// Returns the number of ticks recovered.
fn recover_stuck_ticks(stores: &Stores, bridge: &AgentIpcBridge) -> u32 {
    let stuck_tick_ids: Vec<String> = {
        let ticks = stores.ticks.read().unwrap();
        ticks
            .values()
            .filter(|t| t.status == TickStatus::Sealing || t.status == TickStatus::Validating)
            .map(|t| t.id.clone())
            .collect()
    };

    let mut recovered = 0;
    for tick_id in &stuck_tick_ids {
        // Read current status to determine the transition path
        let current_status = {
            let ticks = stores.ticks.read().unwrap();
            ticks.get(tick_id.as_str()).map(|t| t.status)
        };

        let Some(status) = current_status else {
            continue;
        };

        // Transition through the valid FSM path to reach Failed:
        // Sealing → Validating → Failed
        // Validating → Failed
        let mut ok = true;
        if status == TickStatus::Sealing {
            let resp = bridge.request(
                "tick.transition",
                serde_json::json!({
                    "id": tick_id,
                    "target_status": "Validating",
                    "role": "integrator",
                }),
            );
            if resp.is_error() {
                warn!(
                    "Integrator: failed to recover stuck tick {} (Sealing→Validating): {:?}",
                    tick_id, resp.error
                );
                ok = false;
            }
        }

        if ok {
            let resp = bridge.request(
                "tick.transition",
                serde_json::json!({
                    "id": tick_id,
                    "target_status": "Failed",
                    "role": "integrator",
                }),
            );
            if resp.is_error() {
                warn!(
                    "Integrator: failed to recover stuck tick {} (→Failed): {:?}",
                    tick_id, resp.error
                );
                ok = false;
            }
        }

        if ok {
            info!("Integrator: recovered stuck tick {} → Failed", tick_id);
            recovered += 1;

            // Create a learning about the crash recovery
            let _ = bridge.request(
                "learning.create",
                serde_json::json!({
                    "content": format!("Tick {} was stuck after crash, recovered to Failed", tick_id),
                    "scope": "global",
                    "source_id": tick_id,
                }),
            );
        }
    }
    recovered
}

/// Result of a single integrator cycle.
#[derive(Debug, PartialEq)]
pub enum IntegratorCycleResult {
    /// No work to do (no accepted bundles or tick already in progress).
    Idle,
    /// A Tick was created, validated, and published successfully.
    Published { tick_id: String },
    /// A Tick was created but validation failed.
    ValidationFailed { tick_id: String, log: String },
    /// Some bundles were rejected as stale.
    StaleRejected { count: usize },
    /// Stuck ticks were recovered from a crash.
    Recovered { count: u32 },
}

/// Run a single integrator cycle. This is the core deterministic logic.
///
/// 1. Recover any stuck Ticks.
/// 2. Find Accepted Bundles.
/// 3. Validate preconditions (base_tick_id matches latest published Tick).
/// 4. Reject stale bundles.
/// 5. Create Tick, seal it, run validation, publish or fail.
pub fn run_integrator_cycle(
    stores: &Stores,
    bridge: &AgentIpcBridge,
    config: &IntegratorConfig,
) -> Result<IntegratorCycleResult> {
    // 1. Recover stuck ticks from crash
    let recovered = recover_stuck_ticks(stores, bridge);
    if recovered > 0 {
        return Ok(IntegratorCycleResult::Recovered { count: recovered });
    }

    // 2. Check for in-progress ticks — don't create a new one if one exists
    if has_tick_in_progress(stores) {
        return Ok(IntegratorCycleResult::Idle);
    }

    // 3. Find all Accepted bundles
    let accepted_bundles: Vec<(String, Option<String>)> = {
        let bundles = stores.bundles.read().unwrap();
        bundles
            .values()
            .filter(|b| b.status == BundleStatus::Accepted)
            .map(|b| (b.id.clone(), b.base_tick_id.clone()))
            .collect()
    };

    if accepted_bundles.is_empty() {
        return Ok(IntegratorCycleResult::Idle);
    }

    // 4. Validate-then-mutate: check preconditions before any mutations
    let latest_tick_id = latest_published_tick_id(stores);

    // Partition into valid (matching base_tick_id) and stale
    let mut valid_bundle_ids: Vec<String> = Vec::new();
    let mut stale_bundle_ids: Vec<String> = Vec::new();
    for (id, base) in accepted_bundles {
        if base == latest_tick_id {
            valid_bundle_ids.push(id);
        } else {
            stale_bundle_ids.push(id);
        }
    }

    // 5. Reject stale bundles
    for stale_id in &stale_bundle_ids {
        let resp = bridge.request(
            "bundle.transition",
            serde_json::json!({
                "id": stale_id,
                "target_status": "Rejected",
                "role": "integrator",
            }),
        );
        if resp.is_error() {
            warn!(
                "Integrator: failed to reject stale bundle {}: {:?}",
                stale_id, resp.error
            );
        } else {
            info!("Integrator: rejected stale bundle {}", stale_id);
        }
    }

    if !stale_bundle_ids.is_empty() && valid_bundle_ids.is_empty() {
        return Ok(IntegratorCycleResult::StaleRejected {
            count: stale_bundle_ids.len(),
        });
    }

    if valid_bundle_ids.is_empty() {
        return Ok(IntegratorCycleResult::Idle);
    }

    // 6. Create Tick
    let tick_number = next_tick_number(stores);
    let create_resp = bridge.request("tick.create", serde_json::json!({ "number": tick_number }));
    if create_resp.is_error() {
        return Err(eyre!("Failed to create tick: {:?}", create_resp.error));
    }
    let tick_id = create_resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("tick.create did not return id"))?
        .to_string();

    info!(
        "Integrator: created Tick {} (number {}) with {} bundles",
        tick_id,
        tick_number,
        valid_bundle_ids.len()
    );

    // 7. Transition Tick: Open → Sealing
    let seal_resp = bridge.request(
        "tick.transition",
        serde_json::json!({
            "id": tick_id,
            "target_status": "Sealing",
            "role": "integrator",
        }),
    );
    if seal_resp.is_error() {
        return Err(eyre!("Failed to seal tick {}: {:?}", tick_id, seal_resp.error));
    }

    // 8. Transition bundles: Accepted → Integrating
    for bundle_id in &valid_bundle_ids {
        let resp = bridge.request(
            "bundle.transition",
            serde_json::json!({
                "id": bundle_id,
                "target_status": "Integrating",
                "role": "integrator",
            }),
        );
        if resp.is_error() {
            warn!(
                "Integrator: failed to transition bundle {} to Integrating: {:?}",
                bundle_id, resp.error
            );
        }
    }

    // Update tick with bundle IDs and attempted bundle IDs.
    // Clone-then-drop-then-persist to avoid deadlock.
    // Lock ordering: never hold in-memory RwLock while acquiring Store mutex.
    let tick_to_persist = {
        let mut ticks = stores.ticks.write().unwrap();
        if let Some(tick) = ticks.get_mut(&tick_id) {
            tick.bundle_ids = valid_bundle_ids.clone();
            tick.attempted_bundle_ids = valid_bundle_ids.clone();
            Some(tick.clone())
        } else {
            None
        }
    }; // write lock dropped here

    if let Some(tick) = tick_to_persist
        && let Some(ref store) = stores.store
        && let Err(e) = store.lock().unwrap().update(tick)
    {
        warn!("Failed to persist tick bundle_ids: {}", e);
    }

    // 9. Transition Tick: Sealing → Validating
    let validate_resp = bridge.request(
        "tick.transition",
        serde_json::json!({
            "id": tick_id,
            "target_status": "Validating",
            "role": "integrator",
        }),
    );
    if validate_resp.is_error() {
        return Err(eyre!(
            "Failed to transition tick {} to Validating: {:?}",
            tick_id,
            validate_resp.error
        ));
    }

    // 10. Emit validation.started and run validation commands
    let _ = bridge.event_tx().send(DaemonEvent::validation_started(&tick_id));
    let (passed, validation_log) = run_validation_commands(&config.validation_commands);
    let _ = bridge
        .event_tx()
        .send(DaemonEvent::validation_completed(&tick_id, passed, &validation_log));

    // Update tick with validation log — clone-then-drop-then-persist
    let tick_to_persist = {
        let mut ticks = stores.ticks.write().unwrap();
        if let Some(tick) = ticks.get_mut(&tick_id) {
            tick.validation_log = validation_log.clone();
            Some(tick.clone())
        } else {
            None
        }
    };

    if let Some(tick) = tick_to_persist
        && let Some(ref store) = stores.store
        && let Err(e) = store.lock().unwrap().update(tick)
    {
        warn!("Failed to persist tick validation_log: {}", e);
    }

    // 11. Publish or Fail
    if passed {
        // Get integration SHA
        let sha = get_git_head_sha().unwrap_or_else(|| "unknown".to_string());

        // Transition Tick: Validating → Published
        let pub_resp = bridge.request(
            "tick.transition",
            serde_json::json!({
                "id": tick_id,
                "target_status": "Published",
                "role": "integrator",
            }),
        );
        if pub_resp.is_error() {
            return Err(eyre!("Failed to publish tick {}: {:?}", tick_id, pub_resp.error));
        }

        // Update integration SHA — clone-then-drop-then-persist
        let tick_to_persist = {
            let mut ticks = stores.ticks.write().unwrap();
            if let Some(tick) = ticks.get_mut(&tick_id) {
                tick.integration_sha = Some(sha);
                Some(tick.clone())
            } else {
                None
            }
        };

        if let Some(tick) = tick_to_persist
            && let Some(ref store) = stores.store
            && let Err(e) = store.lock().unwrap().update(tick)
        {
            warn!("Failed to persist tick integration_sha: {}", e);
        }

        // Transition bundles: Integrating → Merged
        for bundle_id in &valid_bundle_ids {
            let resp = bridge.request(
                "bundle.transition",
                serde_json::json!({
                    "id": bundle_id,
                    "target_status": "Merged",
                    "role": "integrator",
                }),
            );
            if resp.is_error() {
                warn!(
                    "Integrator: failed to transition bundle {} to Merged: {:?}",
                    bundle_id, resp.error
                );
            }
        }

        info!("Integrator: Tick {} published successfully", tick_id);
        Ok(IntegratorCycleResult::Published {
            tick_id: tick_id.clone(),
        })
    } else {
        // Transition Tick: Validating → Failed
        let fail_resp = bridge.request(
            "tick.transition",
            serde_json::json!({
                "id": tick_id,
                "target_status": "Failed",
                "role": "integrator",
            }),
        );
        if fail_resp.is_error() {
            error!("Integrator: failed to fail tick {}: {:?}", tick_id, fail_resp.error);
        }

        // Transition bundles: Integrating → Rejected
        for bundle_id in &valid_bundle_ids {
            let resp = bridge.request(
                "bundle.transition",
                serde_json::json!({
                    "id": bundle_id,
                    "target_status": "Rejected",
                    "role": "integrator",
                }),
            );
            if resp.is_error() {
                warn!(
                    "Integrator: failed to reject bundle {} after validation failure: {:?}",
                    bundle_id, resp.error
                );
            }
        }

        // Create a learning about the validation failure
        let _ = bridge.request(
            "learning.create",
            serde_json::json!({
                "content": format!("Tick {} validation failed: {}", tick_id, validation_log),
                "scope": "global",
                "source_id": tick_id,
            }),
        );

        info!("Integrator: Tick {} validation failed", tick_id);
        Ok(IntegratorCycleResult::ValidationFailed {
            tick_id: tick_id.clone(),
            log: validation_log,
        })
    }
}

/// Run the integrator as a long-lived loop. Called from executor.rs.
///
/// This is the main entry point for the Integrator Tokio task.
/// It loops with a configurable interval, running `run_integrator_cycle` each time.
pub async fn run_integrator(
    session: &mut AgentSession,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    config: &IntegratorConfig,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    info!(
        "Integrator {} started (interval: {}s)",
        session.id, config.interval_secs
    );

    let interval = Duration::from_secs(config.interval_secs);

    loop {
        // Check cancellation
        if is_session_cancelled(stores, &session.id) {
            info!("Integrator {} cancelled, exiting loop", session.id);
            return Ok(());
        }

        session.iteration = session.iteration.saturating_add(1);

        match run_integrator_cycle(stores, bridge, config) {
            Ok(result) => {
                let summary = match &result {
                    IntegratorCycleResult::Idle => "idle".to_string(),
                    IntegratorCycleResult::Published { tick_id } => {
                        format!("published tick {}", tick_id)
                    }
                    IntegratorCycleResult::ValidationFailed { tick_id, .. } => {
                        format!("tick {} validation failed", tick_id)
                    }
                    IntegratorCycleResult::StaleRejected { count } => {
                        format!("rejected {} stale bundles", count)
                    }
                    IntegratorCycleResult::Recovered { count } => {
                        format!("recovered {} stuck ticks", count)
                    }
                };

                if result != IntegratorCycleResult::Idle {
                    let _ = event_tx.send(DaemonEvent::new(
                        "integrator.cycle",
                        serde_json::json!({
                            "session_id": session.id,
                            "iteration": session.iteration,
                            "result": summary,
                        }),
                    ));
                }
            }
            Err(e) => {
                error!("Integrator {} cycle error: {}", session.id, e);
                let _ = event_tx.send(DaemonEvent::new(
                    "integrator.error",
                    serde_json::json!({
                        "session_id": session.id,
                        "iteration": session.iteration,
                        "error": e.to_string(),
                    }),
                ));
            }
        }

        // Sleep before next cycle
        tokio::time::sleep(interval).await;
    }
}

/// Run validation commands synchronously (same pattern as handlers.rs).
fn run_validation_commands(commands: &[String]) -> (bool, String) {
    let mut log = String::new();
    for cmd in commands {
        log.push_str(&format!("=== Running: {cmd} ===\n"));
        let output = std::process::Command::new("sh").arg("-c").arg(cmd).output();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stdout.is_empty() {
                    log.push_str(&stdout);
                    if !stdout.ends_with('\n') {
                        log.push('\n');
                    }
                }
                if !stderr.is_empty() {
                    log.push_str(&stderr);
                    if !stderr.ends_with('\n') {
                        log.push('\n');
                    }
                }
                if !out.status.success() {
                    log.push_str(&format!("=== FAILED (exit code {:?}) ===\n", out.status.code()));
                    return (false, log);
                }
                log.push_str("=== PASSED ===\n");
            }
            Err(e) => {
                log.push_str(&format!("=== FAILED to execute: {e} ===\n"));
                return (false, log);
            }
        }
    }
    (true, log)
}

/// Get the current git HEAD SHA.
fn get_git_head_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentType;
    use crate::config::{Config, ProjectConfig};
    use crate::domain::bundle::Bundle;
    use crate::domain::tick::Tick;
    use crate::worktree::manager::WorktreeManager;
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;

    fn test_stores(dir: &std::path::Path) -> Arc<Stores> {
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(StdMutex::new(store)));
        stores.config = config;
        Arc::new(stores)
    }

    fn test_bridge(stores: Arc<Stores>, dir: &std::path::Path) -> (AgentIpcBridge, broadcast::Sender<DaemonEvent>) {
        let (event_tx, _) = broadcast::channel(64);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        (bridge, event_tx)
    }

    fn test_config() -> IntegratorConfig {
        IntegratorConfig {
            validation_commands: vec!["true".to_string()], // always passes
            interval_secs: 1,
            enabled: true,
            session_timeout_secs: None,
        }
    }

    fn failing_config() -> IntegratorConfig {
        IntegratorConfig {
            validation_commands: vec!["false".to_string()], // always fails
            interval_secs: 1,
            enabled: true,
            session_timeout_secs: None,
        }
    }

    // --- is_session_cancelled tests ---

    #[test]
    fn test_is_session_cancelled_false() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-canc1-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Integrator, "model".into());
        let _ = session.transition_to(AgentStatus::Running);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        assert!(!is_session_cancelled(&stores, &sid));
    }

    #[test]
    fn test_is_session_cancelled_true() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-canc2-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Integrator, "model".into());
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Cancelled);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        assert!(is_session_cancelled(&stores, &sid));
    }

    #[test]
    fn test_is_session_cancelled_missing() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-canc3-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        assert!(is_session_cancelled(&stores, "nonexistent-id"));
    }

    // --- Helper function tests ---

    #[test]
    fn test_latest_published_tick_id_none() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-latest1-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        assert!(latest_published_tick_id(&stores).is_none());
    }

    #[test]
    fn test_latest_published_tick_id_some() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-latest2-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut tick1 = Tick::new(1);
        tick1.status = TickStatus::Published;
        let mut tick2 = Tick::new(2);
        tick2.status = TickStatus::Published;
        let tick2_id = tick2.id.clone();
        let mut tick3 = Tick::new(3);
        tick3.status = TickStatus::Failed;

        stores.ticks.write().unwrap().insert(tick1.id.clone(), tick1);
        stores.ticks.write().unwrap().insert(tick2.id.clone(), tick2);
        stores.ticks.write().unwrap().insert(tick3.id.clone(), tick3);

        assert_eq!(latest_published_tick_id(&stores), Some(tick2_id));
    }

    #[test]
    fn test_next_tick_number_empty() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-next1-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        assert_eq!(next_tick_number(&stores), 1);
    }

    #[test]
    fn test_next_tick_number_with_existing() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-next2-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let tick = Tick::new(5);
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        assert_eq!(next_tick_number(&stores), 6);
    }

    #[test]
    fn test_has_tick_in_progress_false() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-tip1-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        assert!(!has_tick_in_progress(&stores));

        // Add a published tick — still no in-progress
        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        assert!(!has_tick_in_progress(&stores));
    }

    #[test]
    fn test_has_tick_in_progress_true() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-tip2-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let tick = Tick::new(1); // status = Open
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        assert!(has_tick_in_progress(&stores));
    }

    // --- recover_stuck_ticks tests ---

    #[test]
    fn test_recover_stuck_ticks_none() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-recov1-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);

        assert_eq!(recover_stuck_ticks(&stores, &bridge), 0);
    }

    #[test]
    fn test_recover_stuck_ticks_sealing() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-recov2-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Sealing;
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        assert_eq!(recover_stuck_ticks(&stores, &bridge), 1);

        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
    }

    #[test]
    fn test_recover_stuck_ticks_validating() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-recov3-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);

        let mut tick = Tick::new(2);
        tick.status = TickStatus::Validating;
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        assert_eq!(recover_stuck_ticks(&stores, &bridge), 1);

        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
    }

    // --- run_integrator_cycle tests ---

    #[test]
    fn test_cycle_idle_no_bundles() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-cycle1-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);
        let config = test_config();

        let result = run_integrator_cycle(&stores, &bridge, &config).unwrap();
        assert_eq!(result, IntegratorCycleResult::Idle);
    }

    #[test]
    fn test_cycle_idle_tick_in_progress() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-cycle2-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);
        let config = test_config();

        // Add a tick in progress
        let tick = Tick::new(1);
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        // Add an accepted bundle
        let bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), "claims".into());
        let mut bundle = bundle;
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = run_integrator_cycle(&stores, &bridge, &config).unwrap();
        assert_eq!(result, IntegratorCycleResult::Idle);
    }

    #[test]
    fn test_cycle_recovers_stuck_tick() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-cycle3-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);
        let config = test_config();

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Sealing;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let result = run_integrator_cycle(&stores, &bridge, &config).unwrap();
        assert_eq!(result, IntegratorCycleResult::Recovered { count: 1 });
    }

    #[test]
    fn test_cycle_publishes_tick() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-cycle4-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);
        let config = test_config(); // "true" command → passes

        // Add an accepted bundle with no base_tick_id (first tick scenario)
        let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), "claims".into());
        bundle.status = BundleStatus::Accepted;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = run_integrator_cycle(&stores, &bridge, &config).unwrap();
        assert!(
            matches!(result, IntegratorCycleResult::Published { .. }),
            "expected Published, got {:?}",
            result
        );

        // Verify bundle transitioned to Merged
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status, BundleStatus::Merged);
    }

    #[test]
    fn test_cycle_validation_failure() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-cycle5-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);
        let config = failing_config(); // "false" command → fails

        // Add an accepted bundle
        let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), "claims".into());
        bundle.status = BundleStatus::Accepted;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = run_integrator_cycle(&stores, &bridge, &config).unwrap();
        assert!(
            matches!(result, IntegratorCycleResult::ValidationFailed { .. }),
            "expected ValidationFailed, got {:?}",
            result
        );

        // Verify bundle transitioned to Rejected
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status, BundleStatus::Rejected);
    }

    #[test]
    fn test_cycle_stale_bundle_rejected() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-cycle6-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);
        let config = test_config();

        // Add a published tick
        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        // Add a bundle with wrong base_tick_id (stale)
        let mut bundle = Bundle::new(
            "wi-1".into(),
            Some("wrong-tick-id".into()),
            "feature/x".into(),
            "claims".into(),
        );
        bundle.status = BundleStatus::Accepted;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = run_integrator_cycle(&stores, &bridge, &config).unwrap();
        assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

        // Verify bundle is rejected
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status, BundleStatus::Rejected);

        // Verify no tick was created (only the published one exists)
        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[&tick_id].status, TickStatus::Published);
    }

    #[test]
    fn test_cycle_mixed_stale_and_valid() {
        let dir = std::env::temp_dir().join(format!("loopr-intg-cycle7-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (bridge, _) = test_bridge(stores.clone(), &dir);
        let config = test_config();

        // Add a published tick
        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        let published_tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        // Valid bundle (correct base_tick_id)
        let mut valid_bundle = Bundle::new(
            "wi-1".into(),
            Some(published_tick_id.clone()),
            "feature/valid".into(),
            "claims".into(),
        );
        valid_bundle.status = BundleStatus::Accepted;
        let valid_id = valid_bundle.id.clone();
        stores
            .bundles
            .write()
            .unwrap()
            .insert(valid_bundle.id.clone(), valid_bundle);

        // Stale bundle (wrong base_tick_id)
        let mut stale_bundle = Bundle::new(
            "wi-2".into(),
            Some("old-tick-id".into()),
            "feature/stale".into(),
            "claims".into(),
        );
        stale_bundle.status = BundleStatus::Accepted;
        let stale_id = stale_bundle.id.clone();
        stores
            .bundles
            .write()
            .unwrap()
            .insert(stale_bundle.id.clone(), stale_bundle);

        let result = run_integrator_cycle(&stores, &bridge, &config).unwrap();
        // Should publish because there's at least one valid bundle
        assert!(
            matches!(result, IntegratorCycleResult::Published { .. }),
            "expected Published, got {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&valid_id].status, BundleStatus::Merged);
        assert_eq!(bundles[&stale_id].status, BundleStatus::Rejected);
    }

    // --- run_validation_commands tests ---

    #[test]
    fn test_validation_commands_pass() {
        let (passed, log) = run_validation_commands(&["true".to_string()]);
        assert!(passed);
        assert!(log.contains("PASSED"));
    }

    #[test]
    fn test_validation_commands_fail() {
        let (passed, log) = run_validation_commands(&["false".to_string()]);
        assert!(!passed);
        assert!(log.contains("FAILED"));
    }

    #[test]
    fn test_validation_commands_empty() {
        let (passed, log) = run_validation_commands(&[]);
        assert!(passed);
        assert!(log.is_empty());
    }

    #[test]
    fn test_validation_commands_multiple_pass() {
        let (passed, _) = run_validation_commands(&["true".to_string(), "true".to_string()]);
        assert!(passed);
    }

    #[test]
    fn test_validation_commands_first_fails() {
        let (passed, log) = run_validation_commands(&["false".to_string(), "true".to_string()]);
        assert!(!passed);
        assert!(log.contains("FAILED"));
    }
}
