//! Crash Recovery - Restores interrupted loops after daemon restart
//!
//! When the daemon crashes or is killed while loops are running, this module
//! handles recovery by:
//! - Finding loops that were marked as Running
//! - Auto-committing work if the worktree exists
//! - Marking loops as Pending for resume, or Failed if worktree lost

use std::sync::Arc;

use crate::domain::{Loop, LoopStatus};
use crate::error::Result;
use crate::storage::StorageWrapper;
use crate::worktree::WorktreeManager;

/// Result of recovering a single loop
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Loop was resumed - worktree existed, changes committed
    Resumed { loop_id: String },
    /// Loop was marked failed - worktree was lost
    MarkedFailed { loop_id: String },
    /// No action needed - loop was not in interrupted state
    Skipped { loop_id: String },
}

/// Configuration for recovery
#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    /// Message to use for auto-commit
    pub commit_message: String,
    /// Whether to auto-commit changes
    pub auto_commit: bool,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            commit_message: "WIP: recovery".to_string(),
            auto_commit: true,
        }
    }
}

/// Recovery manager handles restoring interrupted loops
#[derive(Debug)]
pub struct Recovery {
    storage: Arc<StorageWrapper>,
    worktree_manager: Arc<WorktreeManager>,
    config: RecoveryConfig,
}

impl Recovery {
    /// Create a new recovery manager
    pub fn new(storage: Arc<StorageWrapper>, worktree_manager: Arc<WorktreeManager>, config: RecoveryConfig) -> Self {
        Self {
            storage,
            worktree_manager,
            config,
        }
    }

    /// Create with default config
    pub fn with_defaults(storage: Arc<StorageWrapper>, worktree_manager: Arc<WorktreeManager>) -> Self {
        Self::new(storage, worktree_manager, RecoveryConfig::default())
    }

    /// Recover all interrupted loops
    pub fn recover_all(&self) -> Result<Vec<RecoveryAction>> {
        let loops: Vec<Loop> = self.storage.list_all()?;
        let interrupted: Vec<Loop> = loops.into_iter().filter(|l| l.status == LoopStatus::Running).collect();

        let mut actions = Vec::new();
        for loop_record in interrupted {
            let action = self.recover_loop(&loop_record)?;
            actions.push(action);
        }
        Ok(actions)
    }

    /// Recover a single interrupted loop
    pub fn recover_loop(&self, loop_record: &Loop) -> Result<RecoveryAction> {
        if loop_record.status != LoopStatus::Running {
            return Ok(RecoveryAction::Skipped {
                loop_id: loop_record.id.clone(),
            });
        }

        // Check if worktree still exists
        if self.worktree_manager.exists(&loop_record.id) {
            // Auto-commit any changes if configured
            if self.config.auto_commit {
                // Note: auto_commit returns a Result but we don't need to fail recovery
                // if commit fails (might be nothing to commit)
                let _ = self
                    .worktree_manager
                    .auto_commit(&loop_record.id, &self.config.commit_message);
            }

            // Mark as pending for resume
            let mut updated = loop_record.clone();
            updated.status = LoopStatus::Pending;
            updated.progress.push_str(&format!(
                "\n---\nRecovered at iteration {} after crash\n",
                updated.iteration
            ));
            self.storage.update(&updated)?;

            Ok(RecoveryAction::Resumed {
                loop_id: loop_record.id.clone(),
            })
        } else {
            // Worktree lost, mark as failed
            let mut updated = loop_record.clone();
            updated.status = LoopStatus::Failed;
            updated.progress.push_str("\n---\nFailed: worktree lost during crash\n");
            self.storage.update(&updated)?;

            Ok(RecoveryAction::MarkedFailed {
                loop_id: loop_record.id.clone(),
            })
        }
    }

    /// Count of loops that need recovery
    pub fn count_interrupted(&self) -> Result<usize> {
        let loops: Vec<Loop> = self.storage.list_all()?;
        Ok(loops.iter().filter(|l| l.status == LoopStatus::Running).count())
    }

    /// Check if any loops need recovery
    pub fn needs_recovery(&self) -> Result<bool> {
        Ok(self.count_interrupted()? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageWrapper;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn make_running_loop(id: &str) -> Loop {
        Loop {
            id: id.to_string(),
            loop_type: crate::domain::LoopType::Code,
            parent_id: None,
            input_artifact: None,
            output_artifacts: Vec::new(),
            prompt_path: PathBuf::from("prompts/code.md"),
            validation_command: String::new(),
            max_iterations: 10,
            worktree: PathBuf::from(format!(".loopr/worktrees/test-{}", id)),
            iteration: 3,
            status: LoopStatus::Running,
            progress: "Previous progress".to_string(),
            context: serde_json::Value::Null,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn make_pending_loop(id: &str) -> Loop {
        let mut l = make_running_loop(id);
        l.status = LoopStatus::Pending;
        l
    }

    fn setup_test() -> (TempDir, Arc<StorageWrapper>, Arc<WorktreeManager>) {
        let temp = TempDir::new().unwrap();
        let storage = Arc::new(StorageWrapper::open(temp.path().join("storage")).unwrap());
        let worktree_mgr = Arc::new(WorktreeManager::new(
            temp.path().to_path_buf(),
            temp.path().join("worktrees"),
        ));
        (temp, storage, worktree_mgr)
    }

    #[test]
    fn test_recovery_config_default() {
        let config = RecoveryConfig::default();
        assert_eq!(config.commit_message, "WIP: recovery");
        assert!(config.auto_commit);
    }

    #[test]
    fn test_recovery_new() {
        let (_temp, storage, worktree_mgr) = setup_test();
        let config = RecoveryConfig {
            commit_message: "Custom message".to_string(),
            auto_commit: false,
        };
        let recovery = Recovery::new(storage, worktree_mgr, config);
        assert_eq!(recovery.config.commit_message, "Custom message");
        assert!(!recovery.config.auto_commit);
    }

    #[test]
    fn test_recovery_with_defaults() {
        let (_temp, storage, worktree_mgr) = setup_test();
        let recovery = Recovery::with_defaults(storage, worktree_mgr);
        assert!(recovery.config.auto_commit);
    }

    #[test]
    fn test_recover_loop_skips_non_running() {
        let (_temp, storage, worktree_mgr) = setup_test();
        let recovery = Recovery::with_defaults(storage, worktree_mgr);

        let pending = make_pending_loop("test-1");
        let action = recovery.recover_loop(&pending).unwrap();

        assert_eq!(
            action,
            RecoveryAction::Skipped {
                loop_id: "test-1".to_string()
            }
        );
    }

    #[test]
    fn test_recover_loop_marks_failed_if_worktree_missing() {
        let (_temp, storage, worktree_mgr) = setup_test();
        let recovery = Recovery::with_defaults(Arc::clone(&storage), worktree_mgr);

        let running = make_running_loop("test-2");
        storage.create(&running).unwrap();

        let action = recovery.recover_loop(&running).unwrap();

        assert_eq!(
            action,
            RecoveryAction::MarkedFailed {
                loop_id: "test-2".to_string()
            }
        );

        // Verify status was updated
        let updated: Option<Loop> = storage.get("test-2").unwrap();
        assert!(updated.is_some());
        let updated = updated.unwrap();
        assert_eq!(updated.status, LoopStatus::Failed);
        assert!(updated.progress.contains("worktree lost"));
    }

    #[test]
    fn test_count_interrupted() {
        let (_temp, storage, worktree_mgr) = setup_test();
        let recovery = Recovery::with_defaults(Arc::clone(&storage), worktree_mgr);

        storage.create(&make_running_loop("r1")).unwrap();
        storage.create(&make_running_loop("r2")).unwrap();
        storage.create(&make_pending_loop("p1")).unwrap();

        assert_eq!(recovery.count_interrupted().unwrap(), 2);
    }

    #[test]
    fn test_needs_recovery_true() {
        let (_temp, storage, worktree_mgr) = setup_test();
        let recovery = Recovery::with_defaults(Arc::clone(&storage), worktree_mgr);

        storage.create(&make_running_loop("r1")).unwrap();

        assert!(recovery.needs_recovery().unwrap());
    }

    #[test]
    fn test_needs_recovery_false() {
        let (_temp, storage, worktree_mgr) = setup_test();
        let recovery = Recovery::with_defaults(Arc::clone(&storage), worktree_mgr);

        storage.create(&make_pending_loop("p1")).unwrap();

        assert!(!recovery.needs_recovery().unwrap());
    }

    #[test]
    fn test_recover_all_empty() {
        let (_temp, storage, worktree_mgr) = setup_test();
        let recovery = Recovery::with_defaults(storage, worktree_mgr);

        let actions = recovery.recover_all().unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn test_recover_all_processes_all_running() {
        let (_temp, storage, worktree_mgr) = setup_test();
        let recovery = Recovery::with_defaults(Arc::clone(&storage), worktree_mgr);

        storage.create(&make_running_loop("r1")).unwrap();
        storage.create(&make_running_loop("r2")).unwrap();
        storage.create(&make_pending_loop("p1")).unwrap();

        let actions = recovery.recover_all().unwrap();
        assert_eq!(actions.len(), 2);

        // Both should be marked failed since worktrees don't exist
        assert!(actions.iter().all(|a| matches!(a, RecoveryAction::MarkedFailed { .. })));
    }

    #[test]
    fn test_recovery_action_eq() {
        let a1 = RecoveryAction::Resumed {
            loop_id: "x".to_string(),
        };
        let a2 = RecoveryAction::Resumed {
            loop_id: "x".to_string(),
        };
        assert_eq!(a1, a2);

        let b1 = RecoveryAction::MarkedFailed {
            loop_id: "y".to_string(),
        };
        let b2 = RecoveryAction::Skipped {
            loop_id: "y".to_string(),
        };
        assert_ne!(b1, b2);
    }
}
