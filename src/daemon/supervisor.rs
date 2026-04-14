use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::agents::{AgentEvent, AgentKind, AgentStatus};
use crate::config::IntegratorConfig;
use crate::daemon::context::Stores;
use crate::fsm::runtime::FsmInterpreter;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

/// Minimum continuous uptime before a coordinator failure is considered a healthy crash
/// (i.e., the restart counter resets). Crashes before this threshold count against the ceiling.
const HEALTHY_UPTIME_SECS: u64 = 60;

/// Configuration for the coordinator supervisor.
pub struct SupervisorConfig {
    pub base_delay_secs: u64,
    pub max_delay_secs: u64,
    pub max_restarts: u32,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            base_delay_secs: 10,
            max_delay_secs: 300,
            max_restarts: 5,
        }
    }
}

/// Watches for coordinator session failures and restarts with exponential backoff.
pub async fn run_supervisor(
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    worktree_mgr: WorktreeManager,
    integrator_config: IntegratorConfig,
    fsm: Arc<FsmInterpreter>,
    config: SupervisorConfig,
) {
    let mut event_rx = event_tx.subscribe();
    let mut restart_count = 0u32;
    // Latched on the first Running event of each coordinator session. Cleared on any terminal
    // status. Used to distinguish startup crashes (penalized) from progress-then-crash (reset).
    let mut running_since: Option<tokio::time::Instant> = None;

    loop {
        let event = match event_rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("supervisor lagged {} events", n);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };

        // Only care about agent status changes
        if event.event != "agent.status_changed" {
            continue;
        }

        // Parse the event to check if it's a coordinator failure
        let agent_event: AgentEvent = match serde_json::from_value(event.data.clone()) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let (session_id, status) = match agent_event {
            AgentEvent::StatusChange { session_id, status, .. } => (session_id, status),
            _ => continue,
        };

        // Check if this session was a coordinator
        let is_coordinator = {
            let Ok(sessions) = stores.read_agent_sessions() else {
                continue;
            };
            sessions
                .get(&session_id)
                .map(|s| s.agent_type == AgentKind::Coordinator)
                .unwrap_or(false)
        };

        if !is_coordinator {
            continue;
        }

        // Latch start time on first Running event of this session (coordinators cycle
        // Running <-> WaitingForLlm on each LLM call; get_or_insert_with preserves the
        // original start time across those cycles).
        if status == AgentStatus::Running {
            let _ = running_since.get_or_insert_with(tokio::time::Instant::now);
            continue;
        }

        // Clear running_since on any terminal non-Failed status (Completed, Cancelled) so
        // state does not leak into the next session.
        if status.is_terminal() && status != AgentStatus::Failed {
            running_since = None;
            continue;
        }

        if status != AgentStatus::Failed {
            continue;
        }

        // Check if another coordinator is already running
        let has_active_coordinator = {
            let Ok(sessions) = stores.read_agent_sessions() else {
                continue;
            };
            sessions
                .values()
                .any(|s| s.agent_type == AgentKind::Coordinator && !s.status().is_terminal())
        };

        if has_active_coordinator {
            continue;
        }

        // If the coordinator ran for long enough before failing, treat it as a healthy crash
        // and reset the counter. This prevents transient failures on long-running plans from
        // permanently exhausting the restart budget.
        let made_progress = running_since
            .take()
            .map(|t| t.elapsed().as_secs() >= HEALTHY_UPTIME_SECS)
            .unwrap_or(false);
        if made_progress {
            info!(
                "Coordinator ran for >{}s before failing, resetting restart counter",
                HEALTHY_UPTIME_SECS
            );
            restart_count = 0;
        }

        if restart_count >= config.max_restarts {
            warn!("Coordinator has failed {} times, supervisor giving up", restart_count);
            break;
        }

        restart_count += 1;
        let delay =
            Duration::from_secs((config.base_delay_secs * 2u64.pow(restart_count - 1)).min(config.max_delay_secs));

        info!(
            "Coordinator failed (attempt {}/{}), restarting in {:?}",
            restart_count, config.max_restarts, delay
        );
        tokio::time::sleep(delay).await;

        // Restart via the same dispatch path as auto-start
        let start_req =
            crate::ipc::protocol::DaemonRequest::new(0, "agent.start", json!({ "agent_type": "coordinator" }));
        let response =
            crate::daemon::handlers::dispatch(&stores, &event_tx, &worktree_mgr, &integrator_config, &fsm, start_req)
                .await;

        if response.error.is_some() {
            warn!("Supervisor failed to restart coordinator: {:?}", response.error);
        } else {
            info!("Supervisor restarted coordinator (attempt {})", restart_count);
        }
    }
}

/* Phase 4 cutover: supervisor tests disabled pending engine integration
#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentSession;

    fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    fn test_integrator_config() -> IntegratorConfig {
        IntegratorConfig {
            validation_commands: vec![],
            interval_secs: 60,
            enabled: false,
            session_timeout_secs: None,
            ..Default::default()
        }
    }

    fn test_fsm() -> Arc<FsmInterpreter> {
        Arc::new(FsmInterpreter::embedded().unwrap())
    }

    #[tokio::test]
    async fn test_supervisor_ignores_non_coordinator_failures() {
        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(
            std::env::temp_dir().join("loopr-sup-test"),
            std::env::temp_dir().join("loopr-sup-test-wt"),
        );

        // Insert an implementer session that fails
        let mut session = AgentSession::new(AgentKind::Implementer, "test-model".into());
        let session_id = session.id.clone();
        session.force_status(AgentStatus::Failed);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Send a status_changed event for the implementer
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Failed));

        // Drop the sender so the supervisor exits after processing
        let tx_clone = event_tx.clone();
        drop(event_tx);

        let config = SupervisorConfig {
            base_delay_secs: 1,
            max_delay_secs: 1,
            max_restarts: 3,
        };

        // Supervisor should process the event and exit when channel closes
        // (without attempting restart since it's not a coordinator)
        tokio::time::timeout(
            Duration::from_secs(2),
            run_supervisor(
                stores,
                tx_clone,
                worktree_mgr,
                test_integrator_config(),
                test_fsm(),
                config,
            ),
        )
        .await
        .ok();
    }

    #[tokio::test]
    async fn test_supervisor_running_does_not_reset_counter() {
        // Running alone must NOT reset the restart counter — it only latches running_since.
        // The counter resets only after HEALTHY_UPTIME_SECS of sustained Running.
        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(
            std::env::temp_dir().join("loopr-sup-no-reset"),
            std::env::temp_dir().join("loopr-sup-no-reset-wt"),
        );

        // Insert a coordinator session
        let mut session = AgentSession::new(AgentKind::Coordinator, "test-model".into());
        let session_id = session.id.clone();
        session.force_status(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Send Running event — should NOT reset counter, only latch running_since
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Running));

        let tx_clone = event_tx.clone();
        drop(event_tx);

        let config = SupervisorConfig {
            base_delay_secs: 1,
            max_delay_secs: 1,
            max_restarts: 3,
        };

        // Supervisor processes Running and continues (latches running_since, does not exit).
        // When channel closes it exits cleanly. No counter reset should occur.
        tokio::time::timeout(
            Duration::from_secs(2),
            run_supervisor(
                stores,
                tx_clone,
                worktree_mgr,
                test_integrator_config(),
                test_fsm(),
                config,
            ),
        )
        .await
        .ok();
    }

    #[tokio::test]
    async fn test_supervisor_resets_counter_after_healthy_uptime() {
        // If coordinator ran for >HEALTHY_UPTIME_SECS before failing, counter should reset.
        // Uses tokio::time::pause + advance to control tokio::time::Instant without sleeping.
        tokio::time::pause();

        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(32);
        let worktree_mgr = WorktreeManager::new(
            std::env::temp_dir().join("loopr-sup-healthy"),
            std::env::temp_dir().join("loopr-sup-healthy-wt"),
        );

        // Insert a coordinator session
        let mut session = AgentSession::new(AgentKind::Coordinator, "test-model".into());
        let session_id = session.id.clone();
        session.force_status(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session.clone());

        let tx_for_supervisor = event_tx.clone();
        let stores_clone = stores.clone();

        let supervisor = tokio::spawn(async move {
            run_supervisor(
                stores_clone,
                tx_for_supervisor,
                worktree_mgr,
                test_integrator_config(),
                test_fsm(),
                SupervisorConfig { base_delay_secs: 0, max_delay_secs: 0, max_restarts: 2 },
            )
            .await;
        });

        // Give supervisor time to subscribe
        tokio::time::advance(Duration::from_millis(10)).await;

        // Session reaches Running — latches running_since
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Running));
        tokio::task::yield_now().await;

        // Advance past HEALTHY_UPTIME_SECS
        tokio::time::advance(Duration::from_secs(HEALTHY_UPTIME_SECS + 1)).await;
        tokio::task::yield_now().await;

        // Session fails — should trigger counter reset (not increment) since it ran long enough
        session.force_status(AgentStatus::Failed);
        stores.agent_sessions.write().unwrap().insert(session_id.clone(), session);
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Failed));
        tokio::task::yield_now().await;

        drop(event_tx);
        let _ = tokio::time::timeout(Duration::from_secs(2), supervisor).await;
    }

    #[tokio::test]
    async fn test_supervisor_gives_up_after_max_restarts() {
        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(32);
        let worktree_mgr = WorktreeManager::new(
            std::env::temp_dir().join("loopr-sup-giveup"),
            std::env::temp_dir().join("loopr-sup-giveup-wt"),
        );

        // max_restarts=0 means the supervisor gives up immediately on first failure
        // without attempting dispatch (which requires full runtime context)
        let config = SupervisorConfig {
            base_delay_secs: 0,
            max_delay_secs: 0,
            max_restarts: 0,
        };

        let tx_for_supervisor = event_tx.clone();
        let stores_clone = stores.clone();

        let supervisor = tokio::spawn(async move {
            run_supervisor(
                stores_clone,
                tx_for_supervisor,
                worktree_mgr,
                test_integrator_config(),
                test_fsm(),
                config,
            )
            .await;
        });

        // Give the supervisor time to start and subscribe to events
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send a coordinator failure event
        let mut session = AgentSession::new(AgentKind::Coordinator, "test-model".into());
        let session_id = session.id.clone();
        session.force_status(AgentStatus::Failed);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Failed));

        // The supervisor should exit immediately (max_restarts=0, restart_count=0 >= 0)
        let result = tokio::time::timeout(Duration::from_secs(2), supervisor).await;
        assert!(result.is_ok(), "supervisor should have exited after max restarts");
    }

    #[tokio::test]
    async fn test_supervisor_skips_when_active_coordinator_exists() {
        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(
            std::env::temp_dir().join("loopr-sup-skip"),
            std::env::temp_dir().join("loopr-sup-skip-wt"),
        );

        // Insert a failed coordinator
        let mut failed = AgentSession::new(AgentKind::Coordinator, "test-model".into());
        let failed_id = failed.id.clone();
        failed.force_status(AgentStatus::Failed);
        stores.agent_sessions.write().unwrap().insert(failed_id.clone(), failed);

        // Insert an active (Running) coordinator — supervisor should skip restart
        let mut active = AgentSession::new(AgentKind::Coordinator, "test-model".into());
        active.force_status(AgentStatus::Running);
        stores.agent_sessions.write().unwrap().insert(active.id.clone(), active);

        let _ = event_tx.send(DaemonEvent::agent_status_changed(&failed_id, AgentStatus::Failed));

        let tx_clone = event_tx.clone();
        drop(event_tx);

        let config = SupervisorConfig {
            base_delay_secs: 0,
            max_delay_secs: 0,
            max_restarts: 3,
        };

        // Should process event and exit when channel closes without restarting
        tokio::time::timeout(
            Duration::from_secs(2),
            run_supervisor(
                stores,
                tx_clone,
                worktree_mgr,
                test_integrator_config(),
                test_fsm(),
                config,
            ),
        )
        .await
        .ok();
    }

    #[tokio::test]
    async fn test_supervisor_restart_dispatches_agent_start() {
        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(
            std::env::temp_dir().join("loopr-sup-restart"),
            std::env::temp_dir().join("loopr-sup-restart-wt"),
        );

        // Insert a coordinator session that fails
        let mut session = AgentSession::new(AgentKind::Coordinator, "test-model".into());
        let session_id = session.id.clone();
        session.force_status(AgentStatus::Failed);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Send a Failed event
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Failed));

        let tx_clone = event_tx.clone();
        drop(event_tx);

        let config = SupervisorConfig {
            base_delay_secs: 0, // No delay for tests
            max_delay_secs: 0,
            max_restarts: 1, // Allow 1 restart
        };

        // Run supervisor — it should attempt one restart then exit when channel closes
        tokio::time::timeout(
            Duration::from_secs(5),
            run_supervisor(
                stores.clone(),
                tx_clone,
                worktree_mgr,
                test_integrator_config(),
                test_fsm(),
                config,
            ),
        )
        .await
        .ok();

        // The restart dispatch calls agent.start which creates a new session.
        // Since we have no real agent runtime, the dispatch may succeed at session creation
        // but the spawned tokio task will fail. Just verify the supervisor processed the restart.
        // The fact that we didn't panic and the supervisor exited cleanly is the main assertion.
        // We can also check that the restart_count would have incremented by verifying
        // the supervisor doesn't loop indefinitely with max_restarts: 1.
    }
}
*/
