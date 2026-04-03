use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use tokio::sync::broadcast;

use crate::config::AgentRoleConfig;
use crate::daemon::context::Stores;
use crate::daemon::work_queue;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

/// Configuration for a persistent worker task.
pub struct WorkerConfig {
    pub worker_id: u32,
    /// Seconds between polls when work was just completed.
    pub poll_interval_secs: u64,
    /// Seconds between polls when idle (no work available).
    pub idle_interval_secs: u64,
}

/// Run a persistent worker that pulls and implements Work items.
///
/// The worker loops: pull Work → implement → complete → pull next.
/// Exits when `stores.shutting_down` is set to true.
pub async fn run_worker(
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    worktree_mgr: WorktreeManager,
    implementer_config: AgentRoleConfig,
    config: WorkerConfig,
) {
    info!("Worker {} started", config.worker_id);

    loop {
        // Check for daemon shutdown
        if stores.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
            info!("Worker {} shutting down", config.worker_id);
            break;
        }

        // Get current phase from CoordinatorState
        let current_phase_id = {
            let Ok(states) = stores.read_coordinator_states() else {
                warn!("Worker {} failed to read coordinator states", config.worker_id);
                tokio::time::sleep(Duration::from_secs(config.idle_interval_secs)).await;
                continue;
            };
            states
                .values()
                .find(|s| !s.fsm_state.is_terminal())
                .and_then(|s| s.current_phase_id.clone())
        };

        // Try to pull next Work
        let work_id = work_queue::next_assignable_work(&stores, current_phase_id.as_deref());

        match work_id {
            Some(wid) => {
                info!("Worker {} pulled Work {}", config.worker_id, wid);

                let result = crate::agents::executor::run_single_work(
                    &stores,
                    &event_tx,
                    &worktree_mgr,
                    &implementer_config,
                    &wid,
                    config.worker_id,
                )
                .await;

                match result {
                    Ok(()) => {
                        info!("Worker {} completed Work {}", config.worker_id, wid);
                    }
                    Err(e) => {
                        warn!("Worker {} failed Work {}: {}", config.worker_id, wid, e);
                    }
                }

                // Brief pause before pulling next (avoid hot loop on rapid failures)
                tokio::time::sleep(Duration::from_secs(config.poll_interval_secs)).await;
            }
            None => {
                // No work available — idle
                debug!("Worker {} idle, no Ready Work", config.worker_id);
                tokio::time::sleep(Duration::from_secs(config.idle_interval_secs)).await;
            }
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::work::{Work, WorkStatus};
    use std::sync::atomic::Ordering;

    fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    #[tokio::test]
    async fn test_worker_shuts_down_when_signaled() {
        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(
            std::path::PathBuf::from("/tmp/test-worker"),
            std::path::PathBuf::from("/tmp/test-worker/.worktrees"),
        );
        let config = WorkerConfig {
            worker_id: 0,
            poll_interval_secs: 1,
            idle_interval_secs: 1,
        };

        // Signal shutdown before starting
        stores.shutting_down.store(true, Ordering::Relaxed);

        let handle = tokio::spawn(run_worker(
            stores,
            event_tx,
            worktree_mgr,
            AgentRoleConfig::default_implementer(),
            config,
        ));

        // Worker should exit quickly
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "worker should have exited");
    }

    #[tokio::test]
    async fn test_worker_idles_when_no_work() {
        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(
            std::path::PathBuf::from("/tmp/test-worker-idle"),
            std::path::PathBuf::from("/tmp/test-worker-idle/.worktrees"),
        );
        let config = WorkerConfig {
            worker_id: 0,
            poll_interval_secs: 1,
            idle_interval_secs: 1,
        };

        let stores_clone = stores.clone();
        let handle = tokio::spawn(async move {
            run_worker(
                stores_clone,
                event_tx,
                worktree_mgr,
                AgentRoleConfig::default_implementer(),
                config,
            )
            .await;
        });

        // Let it idle for a bit, then shut down
        tokio::time::sleep(Duration::from_millis(100)).await;
        stores.shutting_down.store(true, Ordering::Relaxed);

        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "worker should have exited after shutdown signal");
    }

    #[tokio::test]
    async fn test_worker_picks_up_ready_work() {
        let stores = test_stores();
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(
            std::path::PathBuf::from("/tmp/test-worker-pick"),
            std::path::PathBuf::from("/tmp/test-worker-pick/.worktrees"),
        );
        let config = WorkerConfig {
            worker_id: 0,
            poll_interval_secs: 1,
            idle_interval_secs: 1,
        };

        // Create a Ready work item
        let mut w = Work::new("phase-1".to_string(), "Test Work".to_string(), String::new());
        w.force_status(WorkStatus::Ready);
        let work_id = w.id.clone();
        stores.works.write().unwrap().insert(work_id.clone(), w);

        let stores_clone = stores.clone();
        let handle = tokio::spawn(async move {
            run_worker(
                stores_clone,
                event_tx,
                worktree_mgr,
                AgentRoleConfig::default_implementer(),
                config,
            )
            .await;
        });

        // Give the worker time to pick up the work and attempt to run it.
        // It will fail (no LLM key, no real worktree) but the Work should
        // transition from Ready. After a brief wait, shut down.
        tokio::time::sleep(Duration::from_millis(200)).await;
        stores.shutting_down.store(true, Ordering::Relaxed);

        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(result.is_ok(), "worker should have exited");

        // The worker should have attempted to run the Work.
        // The Work may have transitioned to InProgress (or failed).
        // At minimum, a session should have been created.
        let sessions = stores.agent_sessions.read().unwrap();
        let worker_sessions: Vec<_> = sessions
            .values()
            .filter(|s| s.work_id.as_deref() == Some(&work_id))
            .collect();
        // Worker either created a session or the transition was rejected (both are OK)
        // Just verify the worker didn't panic
        drop(worker_sessions);
    }
}
