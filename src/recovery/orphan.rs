//! Orphaned worktree detection and cleanup.
//!
//! Handles worktrees that have lost their corresponding loop records:
//! - Finds worktrees without matching loops
//! - Cleans up stale worktrees from crashed sessions
//! - Background cleanup task support

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use eyre::Result;
use log::{info, warn};

use crate::loops::{Worktree, WorktreeConfig, list_worktrees};
use crate::store::TaskStore;

/// Find worktrees that don't have corresponding loop records.
///
/// Returns a list of loop IDs for worktrees that either:
/// - Have no matching loop record
/// - Have a loop record in a terminal state
pub async fn find_orphaned_worktrees(
    store: &Arc<Mutex<TaskStore>>,
    worktree_config: &WorktreeConfig,
) -> Result<Vec<String>> {
    // Get all existing worktree loop IDs
    let worktree_ids = list_worktrees(worktree_config).await?;

    if worktree_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Build set of active loop IDs
    let active_ids: HashSet<String> = {
        let store = store.lock().unwrap();
        store
            .list_all()?
            .into_iter()
            .filter(|r| !r.status.is_terminal())
            .map(|r| r.id)
            .collect()
    };

    // Find orphans
    let orphans: Vec<String> = worktree_ids.into_iter().filter(|id| !active_ids.contains(id)).collect();

    Ok(orphans)
}

/// Find worktrees with terminal (completed/failed) loop records.
///
/// These are worktrees that should have been cleaned up but weren't.
pub async fn find_stale_worktrees(
    store: &Arc<Mutex<TaskStore>>,
    worktree_config: &WorktreeConfig,
) -> Result<Vec<String>> {
    let worktree_ids = list_worktrees(worktree_config).await?;

    if worktree_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut stale = Vec::new();

    let store = store.lock().unwrap();
    for id in worktree_ids {
        if let Some(record) = store.get(&id)?
            && record.status.is_terminal()
        {
            stale.push(id);
        }
    }

    Ok(stale)
}

/// Clean up a single orphaned worktree.
pub async fn cleanup_orphan(loop_id: &str, config: WorktreeConfig) -> Result<()> {
    // Open existing worktree
    let worktree = Worktree::open(loop_id, config)?;

    // Clean it up
    worktree.cleanup().await?;

    info!("Cleaned up orphaned worktree: {}", loop_id);

    Ok(())
}

/// Clean up all orphaned worktrees.
///
/// Returns the number of worktrees cleaned up.
pub async fn cleanup_orphaned_worktrees(
    store: &Arc<Mutex<TaskStore>>,
    worktree_config: &WorktreeConfig,
) -> Result<usize> {
    let orphans = find_orphaned_worktrees(store, worktree_config).await?;
    let count = orphans.len();

    if count == 0 {
        return Ok(0);
    }

    info!("Found {} orphaned worktrees to clean up", count);

    let mut cleaned = 0;
    for loop_id in orphans {
        match cleanup_orphan(&loop_id, worktree_config.clone()).await {
            Ok(()) => cleaned += 1,
            Err(e) => {
                warn!("Failed to clean up orphan {}: {}", loop_id, e);
            }
        }
    }

    Ok(cleaned)
}

/// Clean up worktrees for loops in terminal states.
///
/// Returns the number of worktrees cleaned up.
pub async fn cleanup_stale_worktrees(store: &Arc<Mutex<TaskStore>>, worktree_config: &WorktreeConfig) -> Result<usize> {
    let stale = find_stale_worktrees(store, worktree_config).await?;
    let count = stale.len();

    if count == 0 {
        return Ok(0);
    }

    info!("Found {} stale worktrees to clean up", count);

    let mut cleaned = 0;
    for loop_id in stale {
        match cleanup_orphan(&loop_id, worktree_config.clone()).await {
            Ok(()) => cleaned += 1,
            Err(e) => {
                warn!("Failed to clean up stale worktree {}: {}", loop_id, e);
            }
        }
    }

    Ok(cleaned)
}

/// Background cleanup task that runs periodically.
///
/// This should be spawned as a tokio task and will run indefinitely.
pub async fn background_cleanup_task(
    store: Arc<Mutex<TaskStore>>,
    worktree_config: WorktreeConfig,
    interval_secs: u64,
) {
    let interval = std::time::Duration::from_secs(interval_secs);

    loop {
        tokio::time::sleep(interval).await;

        // Clean up orphans
        match cleanup_orphaned_worktrees(&store, &worktree_config).await {
            Ok(count) if count > 0 => {
                info!("Background cleanup: removed {} orphaned worktrees", count);
            }
            Err(e) => {
                warn!("Background cleanup failed: {}", e);
            }
            _ => {}
        }

        // Clean up stale
        match cleanup_stale_worktrees(&store, &worktree_config).await {
            Ok(count) if count > 0 => {
                info!("Background cleanup: removed {} stale worktrees", count);
            }
            Err(e) => {
                warn!("Stale cleanup failed: {}", e);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{LoopRecord, LoopStatus};
    use tempfile::TempDir;
    use tokio::process::Command;

    fn create_test_store() -> (Arc<Mutex<TaskStore>>, TempDir) {
        let temp = TempDir::new().unwrap();
        let store = TaskStore::open_at(temp.path()).unwrap();
        (Arc::new(Mutex::new(store)), temp)
    }

    async fn setup_test_repo() -> (TempDir, WorktreeConfig) {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path().to_path_buf();

        // Initialize a git repo
        Command::new("git")
            .args(["init"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();

        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();

        tokio::fs::write(repo_path.join("README.md"), "# Test").await.unwrap();

        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();

        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();

        Command::new("git")
            .args(["branch", "-M", "main"])
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();

        let worktree_dir = temp.path().join("worktrees");
        let config = WorktreeConfig::new(repo_path).with_worktree_dir(worktree_dir);

        (temp, config)
    }

    #[tokio::test]
    async fn test_find_orphaned_worktrees_empty() {
        let (store, _temp) = create_test_store();
        let worktree_config = WorktreeConfig::default();

        let orphans = find_orphaned_worktrees(&store, &worktree_config).await.unwrap();

        assert!(orphans.is_empty());
    }

    #[tokio::test]
    async fn test_find_orphaned_worktrees_with_orphan() {
        let (store, _store_temp) = create_test_store();
        let (_repo_temp, worktree_config) = setup_test_repo().await;

        // Create a worktree without a corresponding record
        let worktree = Worktree::create("orphan123", worktree_config.clone()).await.unwrap();

        let orphans = find_orphaned_worktrees(&store, &worktree_config).await.unwrap();

        assert_eq!(orphans.len(), 1);
        assert!(orphans.contains(&"orphan123".to_string()));

        // Cleanup
        worktree.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn test_find_orphaned_worktrees_active_not_orphan() {
        let (store, _store_temp) = create_test_store();
        let (_repo_temp, worktree_config) = setup_test_repo().await;

        // Create a loop record
        let record = {
            let mut s = store.lock().unwrap();
            let mut record = LoopRecord::new_ralph("Test", 5);
            record.status = LoopStatus::Running;
            // Use a specific ID we can match
            record.id = "active123".to_string();
            s.save(&record).unwrap();
            record
        };

        // Create the worktree
        let worktree = Worktree::create(&record.id, worktree_config.clone()).await.unwrap();

        let orphans = find_orphaned_worktrees(&store, &worktree_config).await.unwrap();

        // Should not be an orphan since it has an active record
        assert!(orphans.is_empty());

        // Cleanup
        worktree.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn test_find_stale_worktrees() {
        let (store, _store_temp) = create_test_store();
        let (_repo_temp, worktree_config) = setup_test_repo().await;

        // Create a completed loop record
        {
            let mut s = store.lock().unwrap();
            let mut record = LoopRecord::new_ralph("Test", 5);
            record.status = LoopStatus::Complete;
            record.id = "stale123".to_string();
            s.save(&record).unwrap();
        }

        // Create the worktree (simulating a leftover from before completion)
        let worktree = Worktree::create("stale123", worktree_config.clone()).await.unwrap();

        let stale = find_stale_worktrees(&store, &worktree_config).await.unwrap();

        assert_eq!(stale.len(), 1);
        assert!(stale.contains(&"stale123".to_string()));

        // Cleanup
        worktree.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn test_cleanup_orphaned_worktrees() {
        let (store, _store_temp) = create_test_store();
        let (_repo_temp, worktree_config) = setup_test_repo().await;

        // Create some orphan worktrees
        Worktree::create("orphan1", worktree_config.clone()).await.unwrap();
        Worktree::create("orphan2", worktree_config.clone()).await.unwrap();

        // Clean them up
        let cleaned = cleanup_orphaned_worktrees(&store, &worktree_config).await.unwrap();

        assert_eq!(cleaned, 2);

        // Verify they're gone
        let remaining = list_worktrees(&worktree_config).await.unwrap();
        assert!(remaining.is_empty());
    }
}
