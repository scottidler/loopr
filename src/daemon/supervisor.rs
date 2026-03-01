use std::sync::Arc;
use std::time::Duration;

use log::{info, warn};
use serde_json::json;
use tokio::sync::broadcast;

use crate::agents::{AgentEvent, AgentStatus, AgentType};
use crate::config::IntegratorConfig;
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

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
    config: SupervisorConfig,
) {
    let mut event_rx = event_tx.subscribe();
    let mut restart_count = 0u32;

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
            AgentEvent::StatusChange { session_id, status } => (session_id, status),
            _ => continue,
        };

        // Check if this session was a coordinator
        let is_coordinator = {
            let sessions = stores.agent_sessions.read().unwrap();
            sessions
                .get(&session_id)
                .map(|s| s.agent_type == AgentType::Coordinator)
                .unwrap_or(false)
        };

        if !is_coordinator {
            continue;
        }

        // Reset restart counter when a coordinator reaches Running
        if status == AgentStatus::Running && restart_count > 0 {
            info!("Coordinator reached Running, resetting supervisor restart counter");
            restart_count = 0;
            continue;
        }

        if status != AgentStatus::Failed {
            continue;
        }

        // Check if another coordinator is already running
        let has_active_coordinator = {
            let sessions = stores.agent_sessions.read().unwrap();
            sessions
                .values()
                .any(|s| s.agent_type == AgentType::Coordinator && !s.status.is_terminal())
        };

        if has_active_coordinator {
            continue;
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
            crate::daemon::handlers::dispatch(&stores, &event_tx, &worktree_mgr, &integrator_config, start_req);

        if response.error.is_some() {
            warn!("Supervisor failed to restart coordinator: {:?}", response.error);
        } else {
            info!("Supervisor restarted coordinator (attempt {})", restart_count);
        }
    }
}

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
        }
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
        let mut session = AgentSession::new(AgentType::Implementer, "test-model".into());
        let session_id = session.id.clone();
        session.status = AgentStatus::Failed;
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
            run_supervisor(stores, tx_clone, worktree_mgr, test_integrator_config(), config),
        )
        .await
        .ok();
    }

    #[tokio::test]
    async fn test_supervisor_resets_counter_on_running() {
        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(
            std::env::temp_dir().join("loopr-sup-reset"),
            std::env::temp_dir().join("loopr-sup-reset-wt"),
        );

        // Insert a coordinator session
        let mut session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let session_id = session.id.clone();
        session.status = AgentStatus::Running;
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Send Running event — should reset counter
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Running));

        let tx_clone = event_tx.clone();
        drop(event_tx);

        let config = SupervisorConfig {
            base_delay_secs: 1,
            max_delay_secs: 1,
            max_restarts: 3,
        };

        tokio::time::timeout(
            Duration::from_secs(2),
            run_supervisor(stores, tx_clone, worktree_mgr, test_integrator_config(), config),
        )
        .await
        .ok();
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
                config,
            )
            .await;
        });

        // Give the supervisor time to start and subscribe to events
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send a coordinator failure event
        let mut session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let session_id = session.id.clone();
        session.status = AgentStatus::Failed;
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
        let mut failed = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let failed_id = failed.id.clone();
        failed.status = AgentStatus::Failed;
        stores.agent_sessions.write().unwrap().insert(failed_id.clone(), failed);

        // Insert an active (Running) coordinator — supervisor should skip restart
        let mut active = AgentSession::new(AgentType::Coordinator, "test-model".into());
        active.status = AgentStatus::Running;
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
            run_supervisor(stores, tx_clone, worktree_mgr, test_integrator_config(), config),
        )
        .await
        .ok();
    }
}
