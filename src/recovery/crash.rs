//! Crash recovery for interrupted loops.
//!
//! When Loopr restarts after a crash, this module recovers incomplete loops:
//! 1. Find all loops that were running when the daemon crashed
//! 2. Verify their worktrees still exist
//! 3. Auto-commit any uncommitted work
//! 4. Re-add them to the running queue

use std::sync::{Arc, Mutex};

use eyre::Result;
use log::{info, warn};

use crate::loops::{Worktree, WorktreeConfig};
use crate::store::{LoopRecord, LoopStatus, TaskStore};

/// Configuration for crash recovery.
#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    /// Worktree configuration for accessing existing worktrees.
    pub worktree_config: WorktreeConfig,

    /// Maximum time a loop can be in "running" state without updates
    /// before being considered crashed (in seconds).
    pub stale_threshold_secs: u64,

    /// Message to use for auto-commits during recovery.
    pub auto_commit_message: String,

    /// Whether to auto-commit uncommitted changes during recovery.
    pub auto_commit_enabled: bool,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            worktree_config: WorktreeConfig::default(),
            stale_threshold_secs: 3600, // 1 hour
            auto_commit_message: "WIP: auto-commit before recovery".to_string(),
            auto_commit_enabled: true,
        }
    }
}

impl RecoveryConfig {
    /// Create a new config with the given worktree config.
    pub fn new(worktree_config: WorktreeConfig) -> Self {
        Self {
            worktree_config,
            ..Default::default()
        }
    }

    /// Set the stale threshold.
    pub fn with_stale_threshold(mut self, secs: u64) -> Self {
        self.stale_threshold_secs = secs;
        self
    }

    /// Set the auto-commit message.
    pub fn with_auto_commit_message(mut self, msg: impl Into<String>) -> Self {
        self.auto_commit_message = msg.into();
        self
    }

    /// Enable or disable auto-commit.
    pub fn with_auto_commit(mut self, enabled: bool) -> Self {
        self.auto_commit_enabled = enabled;
        self
    }
}

/// Result of attempting to recover a single loop.
#[derive(Debug, Clone)]
pub enum RecoveryResult {
    /// Loop was successfully recovered and is ready to resume.
    Recovered {
        loop_id: String,
        iteration: u32,
        had_uncommitted_changes: bool,
    },

    /// Worktree was missing, loop marked as failed.
    WorktreeMissing { loop_id: String },

    /// Worktree was corrupted, loop marked as failed.
    WorktreeCorrupted { loop_id: String, error: String },

    /// Loop was already in a terminal state (nothing to recover).
    AlreadyTerminal { loop_id: String, status: LoopStatus },

    /// Recovery failed for another reason.
    Failed { loop_id: String, error: String },
}

impl RecoveryResult {
    /// Check if recovery was successful.
    pub fn is_success(&self) -> bool {
        matches!(self, RecoveryResult::Recovered { .. })
    }

    /// Get the loop ID for this result.
    pub fn loop_id(&self) -> &str {
        match self {
            RecoveryResult::Recovered { loop_id, .. }
            | RecoveryResult::WorktreeMissing { loop_id }
            | RecoveryResult::WorktreeCorrupted { loop_id, .. }
            | RecoveryResult::AlreadyTerminal { loop_id, .. }
            | RecoveryResult::Failed { loop_id, .. } => loop_id,
        }
    }
}

/// Statistics about a recovery operation.
#[derive(Debug, Clone, Default)]
pub struct RecoveryStats {
    /// Number of loops that were recovered.
    pub recovered: usize,

    /// Number of loops with missing worktrees.
    pub missing_worktrees: usize,

    /// Number of loops with corrupted worktrees.
    pub corrupted_worktrees: usize,

    /// Number of loops already in terminal states.
    pub already_terminal: usize,

    /// Number of loops that failed to recover.
    pub failed: usize,

    /// Number of auto-commits performed.
    pub auto_commits: usize,
}

impl RecoveryStats {
    /// Get total number of loops processed.
    pub fn total(&self) -> usize {
        self.recovered + self.missing_worktrees + self.corrupted_worktrees + self.already_terminal + self.failed
    }

    /// Check if all recoverable loops were recovered.
    pub fn is_complete(&self) -> bool {
        self.failed == 0 && self.corrupted_worktrees == 0
    }
}

/// Manager for crash recovery operations.
pub struct RecoveryManager {
    config: RecoveryConfig,
    store: Arc<Mutex<TaskStore>>,
}

impl RecoveryManager {
    /// Create a new recovery manager.
    pub fn new(store: Arc<Mutex<TaskStore>>, config: RecoveryConfig) -> Self {
        Self { config, store }
    }

    /// Find all loops that need recovery.
    ///
    /// These are loops with status "running" that should be checked.
    pub fn find_stale_loops(&self) -> Result<Vec<LoopRecord>> {
        let store = self.store.lock().unwrap();

        // Find all loops with running status
        let running = store.list_by_status(LoopStatus::Running)?;

        // Filter to those that are potentially stale
        // (In a real implementation, we'd check updated_at timestamps)
        Ok(running)
    }

    /// Recover a single loop.
    pub async fn recover_loop(&self, record: &LoopRecord) -> RecoveryResult {
        let loop_id = &record.id;

        // Check if already in terminal state
        if record.status.is_terminal() {
            return RecoveryResult::AlreadyTerminal {
                loop_id: loop_id.clone(),
                status: record.status,
            };
        }

        // Check if worktree exists
        let worktree_path = self.config.worktree_config.worktree_dir.join(loop_id);
        if !worktree_path.exists() {
            warn!("Worktree missing for loop {}, marking as failed", loop_id);

            // Update status to failed
            if let Err(e) = self.mark_failed(loop_id, "Worktree missing after crash") {
                return RecoveryResult::Failed {
                    loop_id: loop_id.clone(),
                    error: format!("Failed to update status: {}", e),
                };
            }

            return RecoveryResult::WorktreeMissing {
                loop_id: loop_id.clone(),
            };
        }

        // Try to open the worktree
        let worktree = match Worktree::open(loop_id, self.config.worktree_config.clone()) {
            Ok(wt) => wt,
            Err(e) => {
                warn!("Cannot open worktree for loop {}: {}", loop_id, e);

                if let Err(e2) = self.mark_failed(loop_id, &format!("Worktree corrupted: {}", e)) {
                    return RecoveryResult::Failed {
                        loop_id: loop_id.clone(),
                        error: format!("Failed to update status: {}", e2),
                    };
                }

                return RecoveryResult::WorktreeCorrupted {
                    loop_id: loop_id.clone(),
                    error: e.to_string(),
                };
            }
        };

        // Check for uncommitted changes
        let had_uncommitted = match worktree.is_clean().await {
            Ok(clean) => !clean,
            Err(e) => {
                return RecoveryResult::WorktreeCorrupted {
                    loop_id: loop_id.clone(),
                    error: format!("Failed to check worktree status: {}", e),
                };
            }
        };

        // Auto-commit if needed
        if had_uncommitted && self.config.auto_commit_enabled {
            info!("Auto-committing uncommitted changes for loop {}", loop_id);

            if let Err(e) = worktree.auto_commit(&self.config.auto_commit_message).await {
                warn!("Failed to auto-commit for loop {}: {}", loop_id, e);
                // Continue anyway - the changes are still there
            }
        }

        // Reset status to pending so it can be re-spawned
        // Keep the iteration count so we continue from where we left off
        if let Err(e) = self.reset_to_pending(loop_id) {
            return RecoveryResult::Failed {
                loop_id: loop_id.clone(),
                error: format!("Failed to reset status: {}", e),
            };
        }

        info!("Recovered loop {} at iteration {}", loop_id, record.iteration);

        RecoveryResult::Recovered {
            loop_id: loop_id.clone(),
            iteration: record.iteration,
            had_uncommitted_changes: had_uncommitted,
        }
    }

    /// Recover all stale loops.
    pub async fn recover_all(&self) -> Result<RecoveryStats> {
        info!("Starting crash recovery...");

        let stale = self.find_stale_loops()?;
        info!("Found {} potentially stale loops", stale.len());

        let mut stats = RecoveryStats::default();

        for record in &stale {
            let result = self.recover_loop(record).await;

            match &result {
                RecoveryResult::Recovered {
                    had_uncommitted_changes,
                    ..
                } => {
                    stats.recovered += 1;
                    if *had_uncommitted_changes {
                        stats.auto_commits += 1;
                    }
                }
                RecoveryResult::WorktreeMissing { .. } => {
                    stats.missing_worktrees += 1;
                }
                RecoveryResult::WorktreeCorrupted { .. } => {
                    stats.corrupted_worktrees += 1;
                }
                RecoveryResult::AlreadyTerminal { .. } => {
                    stats.already_terminal += 1;
                }
                RecoveryResult::Failed { .. } => {
                    stats.failed += 1;
                }
            }
        }

        info!(
            "Recovery complete: {} recovered, {} missing, {} corrupted, {} failed",
            stats.recovered, stats.missing_worktrees, stats.corrupted_worktrees, stats.failed
        );

        Ok(stats)
    }

    /// Mark a loop as failed.
    fn mark_failed(&self, loop_id: &str, reason: &str) -> Result<()> {
        let mut store = self.store.lock().unwrap();

        if let Some(mut record) = store.get(loop_id)? {
            record.status = LoopStatus::Failed;
            record.progress = format!("Crashed: {}", reason);
            record.touch();
            store.update(&record)?;
        }

        Ok(())
    }

    /// Reset a loop to pending status for re-spawning.
    fn reset_to_pending(&self, loop_id: &str) -> Result<()> {
        let mut store = self.store.lock().unwrap();

        if let Some(mut record) = store.get(loop_id)? {
            record.status = LoopStatus::Pending;
            record.progress = format!("Recovered at iteration {}", record.iteration);
            record.touch();
            store.update(&record)?;
        }

        Ok(())
    }
}

/// Check if a loop record is stale (no updates for too long).
pub fn is_stale(record: &LoopRecord, threshold_ms: i64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    now.saturating_sub(record.updated_at) > threshold_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_store() -> (Arc<Mutex<TaskStore>>, TempDir) {
        let temp = TempDir::new().unwrap();
        let store = TaskStore::open_at(temp.path()).unwrap();
        (Arc::new(Mutex::new(store)), temp)
    }

    #[test]
    fn test_recovery_config_default() {
        let config = RecoveryConfig::default();
        assert_eq!(config.stale_threshold_secs, 3600);
        assert!(config.auto_commit_enabled);
    }

    #[test]
    fn test_recovery_config_builder() {
        let config = RecoveryConfig::default()
            .with_stale_threshold(600)
            .with_auto_commit(false)
            .with_auto_commit_message("Test commit");

        assert_eq!(config.stale_threshold_secs, 600);
        assert!(!config.auto_commit_enabled);
        assert_eq!(config.auto_commit_message, "Test commit");
    }

    #[test]
    fn test_recovery_result_is_success() {
        let success = RecoveryResult::Recovered {
            loop_id: "test".to_string(),
            iteration: 1,
            had_uncommitted_changes: false,
        };
        assert!(success.is_success());

        let failure = RecoveryResult::WorktreeMissing {
            loop_id: "test".to_string(),
        };
        assert!(!failure.is_success());
    }

    #[test]
    fn test_recovery_result_loop_id() {
        let result = RecoveryResult::Recovered {
            loop_id: "my-loop".to_string(),
            iteration: 5,
            had_uncommitted_changes: true,
        };
        assert_eq!(result.loop_id(), "my-loop");
    }

    #[test]
    fn test_recovery_stats_default() {
        let stats = RecoveryStats::default();
        assert_eq!(stats.total(), 0);
        assert!(stats.is_complete());
    }

    #[test]
    fn test_recovery_stats_total() {
        let stats = RecoveryStats {
            recovered: 5,
            missing_worktrees: 2,
            corrupted_worktrees: 1,
            already_terminal: 3,
            failed: 1,
            auto_commits: 2,
        };
        assert_eq!(stats.total(), 12);
        assert!(!stats.is_complete()); // has failures
    }

    #[test]
    fn test_find_stale_loops_empty() {
        let (store, _temp) = create_test_store();
        let config = RecoveryConfig::default();
        let manager = RecoveryManager::new(store, config);

        let stale = manager.find_stale_loops().unwrap();
        assert!(stale.is_empty());
    }

    #[test]
    fn test_find_stale_loops_finds_running() {
        let (store, _temp) = create_test_store();

        // Add a running loop
        {
            let mut s = store.lock().unwrap();
            let mut record = LoopRecord::new_ralph("Test", 5);
            record.status = LoopStatus::Running;
            s.save(&record).unwrap();
        }

        let config = RecoveryConfig::default();
        let manager = RecoveryManager::new(store, config);

        let stale = manager.find_stale_loops().unwrap();
        assert_eq!(stale.len(), 1);
    }

    #[test]
    fn test_is_stale() {
        let mut record = LoopRecord::new_ralph("Test", 5);

        // Just updated - not stale
        record.touch();
        assert!(!is_stale(&record, 60000)); // 60 second threshold

        // Set to old timestamp - should be stale
        record.updated_at = 1000; // Very old
        assert!(is_stale(&record, 60000));
    }

    #[tokio::test]
    async fn test_recover_loop_missing_worktree() {
        let (store, temp) = create_test_store();

        // Add a running loop
        let record = {
            let mut s = store.lock().unwrap();
            let mut record = LoopRecord::new_ralph("Test", 5);
            record.status = LoopStatus::Running;
            s.save(&record).unwrap();
            record
        };

        // Use a config that points to the temp dir for worktrees
        let worktree_config =
            WorktreeConfig::new(temp.path().to_path_buf()).with_worktree_dir(temp.path().join("worktrees"));

        let config = RecoveryConfig::new(worktree_config);
        let manager = RecoveryManager::new(store.clone(), config);

        // Try to recover - should fail because worktree doesn't exist
        let result = manager.recover_loop(&record).await;

        assert!(matches!(result, RecoveryResult::WorktreeMissing { .. }));

        // Verify status was updated to failed
        let s = store.lock().unwrap();
        let updated = s.get(&record.id).unwrap().unwrap();
        assert_eq!(updated.status, LoopStatus::Failed);
    }

    #[tokio::test]
    async fn test_recover_loop_already_terminal() {
        let (store, _temp) = create_test_store();

        // Add a completed loop
        let record = {
            let mut s = store.lock().unwrap();
            let mut record = LoopRecord::new_ralph("Test", 5);
            record.status = LoopStatus::Complete;
            s.save(&record).unwrap();
            record
        };

        let config = RecoveryConfig::default();
        let manager = RecoveryManager::new(store, config);

        let result = manager.recover_loop(&record).await;

        assert!(matches!(
            result,
            RecoveryResult::AlreadyTerminal {
                status: LoopStatus::Complete,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_recover_all_empty() {
        let (store, _temp) = create_test_store();
        let config = RecoveryConfig::default();
        let manager = RecoveryManager::new(store, config);

        let stats = manager.recover_all().await.unwrap();
        assert_eq!(stats.total(), 0);
    }
}
