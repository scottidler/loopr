//! Task manager configuration.
//!
//! Global orchestration settings for the LoopManager.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for the task/loop manager.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskManagerConfig {
    /// Maximum concurrent tasks/loops.
    pub max_concurrent_tasks: usize,

    /// Poll interval in seconds (fallback when event-driven fails).
    pub poll_interval_secs: u64,

    /// Shutdown timeout in seconds.
    pub shutdown_timeout_secs: u64,

    /// Repository root path.
    pub repo_root: PathBuf,

    /// Directory for git worktrees.
    pub worktree_dir: PathBuf,
}

impl Default for TaskManagerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_tasks: 50,
            poll_interval_secs: 60,
            shutdown_timeout_secs: 60,
            repo_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            worktree_dir: PathBuf::from("/tmp/loopr/worktrees"),
        }
    }
}

impl TaskManagerConfig {
    /// Create from a GlobalConfig.
    pub fn from_global(global: &super::GlobalConfig) -> Self {
        Self {
            max_concurrent_tasks: global.concurrency.max_loops,
            poll_interval_secs: 60,    // Fixed default
            shutdown_timeout_secs: 60, // Fixed default
            repo_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            worktree_dir: global.git.worktree_dir.clone(),
        }
    }

    /// Validate the configuration.
    pub fn validate(&self) -> eyre::Result<()> {
        if self.max_concurrent_tasks == 0 {
            eyre::bail!("max_concurrent_tasks must be > 0");
        }
        if self.poll_interval_secs == 0 {
            eyre::bail!("poll_interval_secs must be > 0");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default() {
        let config = TaskManagerConfig::default();
        assert_eq!(config.max_concurrent_tasks, 50);
        assert_eq!(config.poll_interval_secs, 60);
    }

    #[test]
    fn test_from_global() {
        let mut global = super::super::GlobalConfig::default();
        global.concurrency.max_loops = 25;
        global.git.worktree_dir = PathBuf::from("/custom/worktrees");

        let config = TaskManagerConfig::from_global(&global);
        assert_eq!(config.max_concurrent_tasks, 25);
        assert_eq!(config.worktree_dir, PathBuf::from("/custom/worktrees"));
    }

    #[test]
    fn test_validation() {
        let config = TaskManagerConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_zero_tasks() {
        let config = TaskManagerConfig {
            max_concurrent_tasks: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
