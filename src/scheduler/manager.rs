//! Loop Manager for orchestrating loop execution.
//!
//! The LoopManager runs a polling loop that:
//! 1. Queries the scheduler for runnable loops
//! 2. Spawns selected loops as async tasks
//! 3. Monitors running loops for completion
//! 4. Handles cleanup and error recovery

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eyre::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::scheduler::rate_limit::RateLimitState;
use crate::scheduler::select::{ConcurrencyConfig, Scheduler};
use crate::store::{LoopRecord, LoopStatus, LoopType, TaskStore};

/// Configuration for the LoopManager.
#[derive(Debug, Clone)]
pub struct LoopManagerConfig {
    /// How often to poll for runnable loops (in seconds).
    pub poll_interval_secs: u64,
    /// Concurrency configuration.
    pub concurrency: ConcurrencyConfig,
}

impl Default for LoopManagerConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 1,
            concurrency: ConcurrencyConfig::default(),
        }
    }
}

impl LoopManagerConfig {
    /// Create a new config with custom poll interval.
    pub fn with_poll_interval(mut self, secs: u64) -> Self {
        self.poll_interval_secs = secs;
        self
    }

    /// Create a new config with custom concurrency settings.
    pub fn with_concurrency(mut self, config: ConcurrencyConfig) -> Self {
        self.concurrency = config;
        self
    }
}

/// Event sent from running loops back to the manager.
#[derive(Debug)]
pub enum LoopEvent {
    /// Loop completed successfully.
    Completed { loop_id: String },
    /// Loop failed.
    Failed { loop_id: String, error: String },
    /// Loop encountered rate limit.
    RateLimited { loop_id: String, retry_after: Duration },
    /// Loop status update.
    StatusUpdate {
        loop_id: String,
        status: LoopStatus,
        iteration: u32,
    },
}

/// Handle to a running loop task.
struct RunningLoop {
    loop_id: String,
    loop_type: LoopType,
    handle: JoinHandle<()>,
}

/// LoopManager orchestrates loop execution.
pub struct LoopManager {
    /// Configuration.
    config: LoopManagerConfig,
    /// Scheduler for selecting loops.
    scheduler: Scheduler,
    /// Task store for persistence.
    store: Arc<Mutex<TaskStore>>,
    /// Global rate limit state.
    rate_limit: Arc<Mutex<RateLimitState>>,
    /// Currently running loop tasks.
    running_loops: HashMap<String, RunningLoop>,
    /// Channel for receiving events from loops.
    event_rx: mpsc::Receiver<LoopEvent>,
    /// Sender for loops to report events.
    event_tx: mpsc::Sender<LoopEvent>,
    /// Whether the manager is running.
    running: bool,
}

impl LoopManager {
    /// Create a new LoopManager.
    pub fn new(store: TaskStore) -> Self {
        let (event_tx, event_rx) = mpsc::channel(100);
        let config = LoopManagerConfig::default();
        let scheduler = Scheduler::new().with_concurrency(config.concurrency.clone());

        Self {
            config,
            scheduler,
            store: Arc::new(Mutex::new(store)),
            rate_limit: Arc::new(Mutex::new(RateLimitState::new())),
            running_loops: HashMap::new(),
            event_rx,
            event_tx,
            running: false,
        }
    }

    /// Create a new LoopManager with custom configuration.
    pub fn with_config(store: TaskStore, config: LoopManagerConfig) -> Self {
        let (event_tx, event_rx) = mpsc::channel(100);
        let scheduler = Scheduler::new().with_concurrency(config.concurrency.clone());

        Self {
            config,
            scheduler,
            store: Arc::new(Mutex::new(store)),
            rate_limit: Arc::new(Mutex::new(RateLimitState::new())),
            running_loops: HashMap::new(),
            event_rx,
            event_tx,
            running: false,
        }
    }

    /// Get the event sender for loops to report back.
    pub fn event_sender(&self) -> mpsc::Sender<LoopEvent> {
        self.event_tx.clone()
    }

    /// Get a reference to the shared rate limit state.
    pub fn rate_limit(&self) -> Arc<Mutex<RateLimitState>> {
        self.rate_limit.clone()
    }

    /// Get a reference to the shared task store.
    pub fn store(&self) -> Arc<Mutex<TaskStore>> {
        self.store.clone()
    }

    /// Get the number of currently running loops.
    pub fn running_count(&self) -> usize {
        self.running_loops.len()
    }

    /// Get running loop counts by type.
    pub fn running_by_type(&self) -> HashMap<LoopType, usize> {
        let mut counts = HashMap::new();
        for rl in self.running_loops.values() {
            *counts.entry(rl.loop_type).or_insert(0) += 1;
        }
        counts
    }

    /// Check if a specific loop is running.
    pub fn is_loop_running(&self, loop_id: &str) -> bool {
        self.running_loops.contains_key(loop_id)
    }

    /// Run the manager's main loop.
    ///
    /// This method runs until `stop()` is called.
    pub async fn run(&mut self) -> Result<()> {
        self.running = true;

        while self.running {
            // Process any pending events from loops
            self.process_events().await?;

            // Select and spawn new loops
            self.tick().await?;

            // Sleep until next poll
            tokio::time::sleep(Duration::from_secs(self.config.poll_interval_secs)).await;
        }

        Ok(())
    }

    /// Stop the manager.
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Perform one scheduling tick.
    ///
    /// This is called periodically to:
    /// 1. Check for completed loops
    /// 2. Select new loops to run
    /// 3. Spawn selected loops
    pub async fn tick(&mut self) -> Result<()> {
        // Reap completed tasks
        self.reap_completed().await?;

        // Get rate limit state and select runnable loops
        let to_start = {
            let rate_limit = self.rate_limit.lock().unwrap();
            let store = self.store.lock().unwrap();
            self.scheduler
                .select_runnable(&store, self.running_loops.len(), Some(&*rate_limit))
        };

        // Spawn each selected loop
        for record in to_start {
            self.spawn_loop(record).await?;
        }

        Ok(())
    }

    /// Process events from running loops.
    async fn process_events(&mut self) -> Result<()> {
        // Non-blocking receive of all pending events
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                LoopEvent::Completed { loop_id } => {
                    tracing::info!(loop_id = %loop_id, "Loop completed");
                    self.running_loops.remove(&loop_id);
                }
                LoopEvent::Failed { loop_id, error } => {
                    tracing::error!(loop_id = %loop_id, error = %error, "Loop failed");
                    self.running_loops.remove(&loop_id);
                }
                LoopEvent::RateLimited { loop_id, retry_after } => {
                    tracing::warn!(
                        loop_id = %loop_id,
                        retry_after_secs = retry_after.as_secs(),
                        "Loop hit rate limit"
                    );
                    let mut rl = self.rate_limit.lock().unwrap();
                    rl.record_rate_limit(retry_after);
                }
                LoopEvent::StatusUpdate {
                    loop_id,
                    status,
                    iteration,
                } => {
                    tracing::debug!(
                        loop_id = %loop_id,
                        status = ?status,
                        iteration = iteration,
                        "Loop status update"
                    );
                }
            }
        }

        Ok(())
    }

    /// Reap completed loop tasks.
    async fn reap_completed(&mut self) -> Result<()> {
        let mut completed = Vec::new();

        for (id, rl) in &self.running_loops {
            if rl.handle.is_finished() {
                completed.push(id.clone());
            }
        }

        for id in completed {
            if let Some(rl) = self.running_loops.remove(&id) {
                // Await the handle to get any panic info
                if let Err(e) = rl.handle.await {
                    tracing::error!(loop_id = %id, error = ?e, "Loop task panicked");
                }
            }
        }

        Ok(())
    }

    /// Spawn a loop as an async task.
    ///
    /// The actual loop execution is handled elsewhere (in the loops module).
    /// This method just updates the record status and tracks the task handle.
    async fn spawn_loop(&mut self, record: LoopRecord) -> Result<()> {
        let loop_id = record.id.clone();
        let loop_type = record.loop_type;

        // Update record status to running
        {
            let mut store = self.store.lock().unwrap();
            let mut updated = record;
            updated.status = LoopStatus::Running;
            updated.touch();
            store.update(&updated)?;
        }

        // Create a placeholder task that just completes
        // In a real implementation, this would run the actual loop logic
        let event_tx = self.event_tx.clone();
        let id_clone = loop_id.clone();

        let handle = tokio::spawn(async move {
            // Placeholder: in real implementation, this would run RalphLoop/PhaseLoop/etc.
            // For now, just signal completion after a brief delay (for testing)
            tokio::time::sleep(Duration::from_millis(100)).await;

            let _ = event_tx.send(LoopEvent::Completed { loop_id: id_clone }).await;
        });

        self.running_loops.insert(
            loop_id.clone(),
            RunningLoop {
                loop_id,
                loop_type,
                handle,
            },
        );

        Ok(())
    }

    /// Cancel a running loop.
    pub async fn cancel_loop(&mut self, loop_id: &str) -> Result<bool> {
        if let Some(rl) = self.running_loops.remove(loop_id) {
            rl.handle.abort();

            // Update record status
            let mut store = self.store.lock().unwrap();
            if let Some(mut record) = store.get(loop_id)? {
                record.status = LoopStatus::Invalidated;
                record.touch();
                store.update(&record)?;
            }

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Cancel all running loops.
    pub async fn cancel_all(&mut self) -> Result<usize> {
        let count = self.running_loops.len();

        for (_, rl) in self.running_loops.drain() {
            rl.handle.abort();
        }

        Ok(count)
    }

    /// Handle orphaned loops (parent was deleted or invalidated).
    pub async fn handle_orphans(&mut self) -> Result<usize> {
        let mut orphan_count = 0;

        // Find all loops with missing parents
        let all_loops = {
            let store = self.store.lock().unwrap();
            store.list_all()?
        };

        for record in all_loops {
            if let Some(ref parent_id) = record.parent_loop {
                let parent = {
                    let store = self.store.lock().unwrap();
                    store.get(parent_id)?
                };

                let is_orphan = match parent {
                    None => true,
                    Some(p) => p.status == LoopStatus::Invalidated || p.status == LoopStatus::Failed,
                };

                if is_orphan && record.status != LoopStatus::Invalidated {
                    orphan_count += 1;

                    // Cancel if running
                    if self.is_loop_running(&record.id) {
                        self.cancel_loop(&record.id).await?;
                    } else {
                        // Just update status
                        let mut store = self.store.lock().unwrap();
                        let mut updated = record;
                        updated.status = LoopStatus::Invalidated;
                        updated.touch();
                        store.update(&updated)?;
                    }
                }
            }
        }

        Ok(orphan_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_temp_store() -> (TaskStore, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let store = TaskStore::open_at(temp_dir.path()).unwrap();
        (store, temp_dir)
    }

    #[test]
    fn test_loop_manager_config_default() {
        let config = LoopManagerConfig::default();
        assert_eq!(config.poll_interval_secs, 1);
    }

    #[test]
    fn test_loop_manager_config_builder() {
        let config = LoopManagerConfig::default()
            .with_poll_interval(5)
            .with_concurrency(ConcurrencyConfig::new(100, 20));

        assert_eq!(config.poll_interval_secs, 5);
        assert_eq!(config.concurrency.max_loops, 100);
    }

    #[tokio::test]
    async fn test_loop_manager_new() {
        let (store, _temp) = create_temp_store();
        let manager = LoopManager::new(store);

        assert_eq!(manager.running_count(), 0);
        assert!(!manager.running);
    }

    #[tokio::test]
    async fn test_loop_manager_spawn_and_complete() {
        let (mut store, _temp) = create_temp_store();

        let record = LoopRecord::new_ralph("Test", 5);
        store.save(&record).unwrap();

        let mut manager = LoopManager::new(store);

        // Run a tick
        manager.tick().await.unwrap();

        // Loop should be spawned
        assert_eq!(manager.running_count(), 1);

        // Wait for it to complete
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Process events and reap
        manager.process_events().await.unwrap();
        manager.reap_completed().await.unwrap();

        assert_eq!(manager.running_count(), 0);
    }

    #[tokio::test]
    async fn test_loop_manager_cancel_loop() {
        let (mut store, _temp) = create_temp_store();

        let record = LoopRecord::new_ralph("Test", 5);
        let loop_id = record.id.clone();
        store.save(&record).unwrap();

        let mut manager = LoopManager::new(store);

        // Spawn the loop
        manager.tick().await.unwrap();
        assert!(manager.is_loop_running(&loop_id));

        // Cancel it
        let cancelled = manager.cancel_loop(&loop_id).await.unwrap();
        assert!(cancelled);
        assert!(!manager.is_loop_running(&loop_id));
    }

    #[tokio::test]
    async fn test_loop_manager_cancel_all() {
        let (mut store, _temp) = create_temp_store();

        for i in 0..5 {
            let record = LoopRecord::new_ralph(&format!("Test {}", i), 5);
            store.save(&record).unwrap();
        }

        let mut manager = LoopManager::new(store);

        // Spawn all
        manager.tick().await.unwrap();
        assert_eq!(manager.running_count(), 5);

        // Cancel all
        let cancelled = manager.cancel_all().await.unwrap();
        assert_eq!(cancelled, 5);
        assert_eq!(manager.running_count(), 0);
    }

    #[tokio::test]
    async fn test_loop_manager_running_by_type() {
        let (mut store, _temp) = create_temp_store();

        store.save(&LoopRecord::new_ralph("Ralph 1", 5)).unwrap();
        store.save(&LoopRecord::new_ralph("Ralph 2", 5)).unwrap();
        store.save(&LoopRecord::new_plan("Plan 1", 10)).unwrap();

        let mut manager = LoopManager::new(store);
        manager.tick().await.unwrap();

        let counts = manager.running_by_type();
        assert_eq!(counts.get(&LoopType::Ralph), Some(&2));
        assert_eq!(counts.get(&LoopType::Plan), Some(&1));
    }

    #[tokio::test]
    async fn test_loop_manager_event_sender() {
        let (store, _temp) = create_temp_store();
        let manager = LoopManager::new(store);

        let sender = manager.event_sender();
        let event = LoopEvent::Completed {
            loop_id: "test".to_string(),
        };
        sender.send(event).await.unwrap();
    }

    #[tokio::test]
    async fn test_loop_manager_rate_limit_blocks_spawning() {
        let (mut store, _temp) = create_temp_store();

        let record = LoopRecord::new_ralph("Test", 5);
        store.save(&record).unwrap();

        let mut manager = LoopManager::new(store);

        // Set rate limit
        {
            let mut rl = manager.rate_limit.lock().unwrap();
            rl.record_rate_limit(Duration::from_secs(60));
        }

        // Tick should not spawn anything
        manager.tick().await.unwrap();
        assert_eq!(manager.running_count(), 0);

        // Clear rate limit
        {
            let mut rl = manager.rate_limit.lock().unwrap();
            rl.record_success();
        }

        // Now it should spawn
        manager.tick().await.unwrap();
        assert_eq!(manager.running_count(), 1);
    }

    #[tokio::test]
    async fn test_loop_manager_handle_orphans() {
        let (mut store, _temp) = create_temp_store();

        // Create a parent that's invalidated
        let mut parent = LoopRecord::new_plan("Parent", 10);
        parent.status = LoopStatus::Invalidated;
        store.save(&parent).unwrap();

        // Create a child
        let child = LoopRecord::new_spec(&parent.id, "Content", 10);
        store.save(&child).unwrap();

        let mut manager = LoopManager::new(store);
        let orphan_count = manager.handle_orphans().await.unwrap();

        assert_eq!(orphan_count, 1);

        // Verify child is now invalidated
        let store = manager.store.lock().unwrap();
        let updated_child = store.get(&child.id).unwrap().unwrap();
        assert_eq!(updated_child.status, LoopStatus::Invalidated);
    }
}
