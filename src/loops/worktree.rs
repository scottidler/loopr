//! Git worktree management for isolated loop execution.
//!
//! Each loop executes in its own git worktree on a feature branch, enabling
//! parallel work without file conflicts. When loops complete or crash,
//! worktrees are cleaned up.

use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::process::Command;

/// Worktree management errors
#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("Failed to create worktree: {0}")]
    CreationFailed(String),

    #[error("Failed to remove worktree: {0}")]
    RemovalFailed(String),

    #[error("Worktree not found at {0}")]
    NotFound(PathBuf),

    #[error("Git command failed: {0}")]
    GitError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid repo path: {0}")]
    InvalidRepoPath(String),
}

/// Configuration for worktree management
#[derive(Debug, Clone)]
pub struct WorktreeConfig {
    /// Base directory for worktrees (e.g., /tmp/loopr/worktrees)
    pub worktree_dir: PathBuf,

    /// Root of the main git repository
    pub repo_root: PathBuf,

    /// Base branch to create worktrees from (default: "main")
    pub base_branch: String,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            worktree_dir: std::env::temp_dir().join("loopr").join("worktrees"),
            repo_root: PathBuf::from("."),
            base_branch: "main".to_string(),
        }
    }
}

impl WorktreeConfig {
    /// Create a new config with the given repo root
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            ..Default::default()
        }
    }

    /// Set the worktree directory
    pub fn with_worktree_dir(mut self, dir: PathBuf) -> Self {
        self.worktree_dir = dir;
        self
    }

    /// Set the base branch
    pub fn with_base_branch(mut self, branch: impl Into<String>) -> Self {
        self.base_branch = branch.into();
        self
    }
}

/// Represents a git worktree for a loop
#[derive(Debug)]
pub struct Worktree {
    /// The loop ID this worktree belongs to
    pub loop_id: String,

    /// Path to the worktree directory
    pub path: PathBuf,

    /// Branch name for this worktree
    pub branch: String,

    /// Configuration
    config: WorktreeConfig,
}

impl Worktree {
    /// Create a new worktree for the given loop ID
    ///
    /// This creates:
    /// 1. A new branch from the base branch (e.g., `loop-1737802800`)
    /// 2. A worktree at `{worktree_dir}/{loop_id}`
    pub async fn create(loop_id: &str, config: WorktreeConfig) -> Result<Self, WorktreeError> {
        let worktree_path = config.worktree_dir.join(loop_id);
        let branch_name = format!("loop-{}", loop_id);

        // Ensure worktree directory parent exists
        if let Some(parent) = worktree_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Create the git worktree
        let output = Command::new("git")
            .args([
                "worktree",
                "add",
                worktree_path
                    .to_str()
                    .ok_or_else(|| WorktreeError::InvalidRepoPath(worktree_path.display().to_string()))?,
                "-b",
                &branch_name,
                &config.base_branch,
            ])
            .current_dir(&config.repo_root)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::CreationFailed(stderr.to_string()));
        }

        Ok(Self {
            loop_id: loop_id.to_string(),
            path: worktree_path,
            branch: branch_name,
            config,
        })
    }

    /// Open an existing worktree
    pub fn open(loop_id: &str, config: WorktreeConfig) -> Result<Self, WorktreeError> {
        let worktree_path = config.worktree_dir.join(loop_id);
        let branch_name = format!("loop-{}", loop_id);

        if !worktree_path.exists() {
            return Err(WorktreeError::NotFound(worktree_path));
        }

        Ok(Self {
            loop_id: loop_id.to_string(),
            path: worktree_path,
            branch: branch_name,
            config,
        })
    }

    /// Check if the worktree has uncommitted changes
    pub async fn is_clean(&self) -> Result<bool, WorktreeError> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&self.path)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::GitError(stderr.to_string()));
        }

        Ok(output.stdout.is_empty())
    }

    /// Auto-commit any uncommitted changes (for crash recovery)
    pub async fn auto_commit(&self, message: &str) -> Result<(), WorktreeError> {
        // Stage all changes
        let add_output = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&self.path)
            .output()
            .await?;

        if !add_output.status.success() {
            let stderr = String::from_utf8_lossy(&add_output.stderr);
            return Err(WorktreeError::GitError(format!("git add failed: {}", stderr)));
        }

        // Commit (allow empty for cases where add staged nothing)
        let commit_output = Command::new("git")
            .args(["commit", "-m", message, "--allow-empty"])
            .current_dir(&self.path)
            .output()
            .await?;

        if !commit_output.status.success() {
            let stderr = String::from_utf8_lossy(&commit_output.stderr);
            // "nothing to commit" is not an error
            if !stderr.contains("nothing to commit") {
                return Err(WorktreeError::GitError(format!("git commit failed: {}", stderr)));
            }
        }

        Ok(())
    }

    /// Get the path to a file within this worktree
    pub fn file_path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.path.join(relative)
    }

    /// Remove this worktree and its branch
    pub async fn cleanup(self) -> Result<(), WorktreeError> {
        // Remove worktree
        let remove_output = Command::new("git")
            .args([
                "worktree",
                "remove",
                self.path
                    .to_str()
                    .ok_or_else(|| WorktreeError::InvalidRepoPath(self.path.display().to_string()))?,
                "--force",
            ])
            .current_dir(&self.config.repo_root)
            .output()
            .await?;

        if !remove_output.status.success() {
            let stderr = String::from_utf8_lossy(&remove_output.stderr);
            // Log warning but continue to try deleting the branch
            log::warn!("Failed to remove worktree: {}", stderr);
        }

        // Delete the branch
        let branch_output = Command::new("git")
            .args(["branch", "-D", &self.branch])
            .current_dir(&self.config.repo_root)
            .output()
            .await?;

        if !branch_output.status.success() {
            let stderr = String::from_utf8_lossy(&branch_output.stderr);
            // Branch might already be deleted
            if !stderr.contains("not found") {
                log::warn!("Failed to delete branch {}: {}", self.branch, stderr);
            }
        }

        Ok(())
    }

    /// Check if a worktree exists for the given loop ID
    pub fn exists(loop_id: &str, config: &WorktreeConfig) -> bool {
        config.worktree_dir.join(loop_id).exists()
    }
}

/// List all worktree directories
pub async fn list_worktrees(config: &WorktreeConfig) -> Result<Vec<String>, WorktreeError> {
    if !config.worktree_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = tokio::fs::read_dir(&config.worktree_dir).await?;
    let mut loop_ids = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            loop_ids.push(name.to_string());
        }
    }

    Ok(loop_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

        // Create initial commit
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

        // Create main branch if not already there
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
    async fn test_worktree_create() {
        let (_temp, config) = setup_test_repo().await;

        let worktree = Worktree::create("test123", config).await.unwrap();

        assert!(worktree.path.exists());
        assert_eq!(worktree.loop_id, "test123");
        assert_eq!(worktree.branch, "loop-test123");
    }

    #[tokio::test]
    async fn test_worktree_open() {
        let (_temp, config) = setup_test_repo().await;

        // First create it
        let created = Worktree::create("open123", config.clone()).await.unwrap();

        // Then open it
        let opened = Worktree::open("open123", config).unwrap();

        assert_eq!(created.path, opened.path);
        assert_eq!(created.branch, opened.branch);
    }

    #[tokio::test]
    async fn test_worktree_is_clean() {
        let (_temp, config) = setup_test_repo().await;

        let worktree = Worktree::create("clean123", config).await.unwrap();

        // Should be clean initially
        assert!(worktree.is_clean().await.unwrap());

        // Create a file to make it dirty
        tokio::fs::write(worktree.file_path("new_file.txt"), "content")
            .await
            .unwrap();

        // Should now be dirty
        assert!(!worktree.is_clean().await.unwrap());
    }

    #[tokio::test]
    async fn test_worktree_auto_commit() {
        let (_temp, config) = setup_test_repo().await;

        let worktree = Worktree::create("commit123", config).await.unwrap();

        // Create a file
        tokio::fs::write(worktree.file_path("new_file.txt"), "content")
            .await
            .unwrap();

        // Auto-commit
        worktree.auto_commit("WIP: auto-commit").await.unwrap();

        // Should be clean now
        assert!(worktree.is_clean().await.unwrap());
    }

    #[tokio::test]
    async fn test_worktree_cleanup() {
        let (_temp, config) = setup_test_repo().await;

        let worktree = Worktree::create("cleanup123", config.clone()).await.unwrap();
        let path = worktree.path.clone();

        // Cleanup
        worktree.cleanup().await.unwrap();

        // Path should no longer exist
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_worktree_exists() {
        let (_temp, config) = setup_test_repo().await;

        assert!(!Worktree::exists("exists123", &config));

        let _worktree = Worktree::create("exists123", config.clone()).await.unwrap();

        assert!(Worktree::exists("exists123", &config));
    }

    #[tokio::test]
    async fn test_list_worktrees() {
        let (_temp, config) = setup_test_repo().await;

        // Initially empty
        let list = list_worktrees(&config).await.unwrap();
        assert!(list.is_empty());

        // Create a few worktrees
        let _w1 = Worktree::create("list1", config.clone()).await.unwrap();
        let _w2 = Worktree::create("list2", config.clone()).await.unwrap();

        let list = list_worktrees(&config).await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.contains(&"list1".to_string()));
        assert!(list.contains(&"list2".to_string()));
    }

    #[tokio::test]
    async fn test_worktree_file_path() {
        let (_temp, config) = setup_test_repo().await;

        let worktree = Worktree::create("path123", config).await.unwrap();

        let file_path = worktree.file_path("src/main.rs");
        assert!(file_path.ends_with("src/main.rs"));
        assert!(file_path.starts_with(&worktree.path));
    }

    #[test]
    fn test_worktree_config_default() {
        let config = WorktreeConfig::default();
        assert!(config.worktree_dir.to_string_lossy().contains("worktrees"));
        assert_eq!(config.base_branch, "main");
    }

    #[test]
    fn test_worktree_config_builder() {
        let config = WorktreeConfig::new(PathBuf::from("/repo"))
            .with_worktree_dir(PathBuf::from("/tmp/wt"))
            .with_base_branch("develop");

        assert_eq!(config.repo_root, PathBuf::from("/repo"));
        assert_eq!(config.worktree_dir, PathBuf::from("/tmp/wt"));
        assert_eq!(config.base_branch, "develop");
    }
}
