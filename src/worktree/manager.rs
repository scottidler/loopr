//! WorktreeManager handles git worktree operations for loop isolation.

use crate::error::{LooprError, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Manages git worktrees for loop isolation.
///
/// Each loop gets its own worktree with a dedicated branch, enabling
/// parallel development without conflicts.
#[derive(Debug)]
pub struct WorktreeManager {
    /// Base path where worktrees are created
    base_path: PathBuf,
    /// Main repository root
    repo_root: PathBuf,
}

impl WorktreeManager {
    /// Create a new WorktreeManager.
    ///
    /// # Arguments
    /// * `base_path` - Directory where worktrees will be created
    /// * `repo_root` - Path to the main git repository
    pub fn new(base_path: impl Into<PathBuf>, repo_root: impl Into<PathBuf>) -> Self {
        Self {
            base_path: base_path.into(),
            repo_root: repo_root.into(),
        }
    }

    /// Create a worktree with a new branch for the given loop.
    ///
    /// Creates the worktree at `{base_path}/{loop_id}` with a branch
    /// named `loop/{loop_id}` branched from main.
    pub fn create(&self, loop_id: &str) -> Result<PathBuf> {
        let worktree_path = self.path(loop_id);
        let branch_name = self.branch_name(loop_id);

        // Ensure base path exists
        std::fs::create_dir_all(&self.base_path)
            .map_err(|e| LooprError::Worktree(format!("Failed to create base path: {}", e)))?;

        // Create worktree with new branch from main
        let output = Command::new("git")
            .args([
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "-b",
                &branch_name,
                "main",
            ])
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| LooprError::Worktree(format!("Failed to execute git: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(LooprError::Worktree(format!("Failed to create worktree: {}", stderr)));
        }

        Ok(worktree_path)
    }

    /// Remove a worktree and optionally delete its branch.
    ///
    /// # Arguments
    /// * `loop_id` - The loop ID whose worktree to remove
    /// * `preserve_branch` - If true, keep the branch after removing worktree
    pub fn cleanup(&self, loop_id: &str, preserve_branch: bool) -> Result<()> {
        let worktree_path = self.path(loop_id);
        let branch_name = self.branch_name(loop_id);

        // Remove worktree (force to handle uncommitted changes)
        if worktree_path.exists() {
            let output = Command::new("git")
                .args(["worktree", "remove", worktree_path.to_str().unwrap(), "--force"])
                .current_dir(&self.repo_root)
                .output()
                .map_err(|e| LooprError::Worktree(format!("Failed to execute git: {}", e)))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(LooprError::Worktree(format!("Failed to remove worktree: {}", stderr)));
            }
        }

        // Delete branch if not preserving
        if !preserve_branch {
            let output = Command::new("git")
                .args(["branch", "-D", &branch_name])
                .current_dir(&self.repo_root)
                .output()
                .map_err(|e| LooprError::Worktree(format!("Failed to execute git: {}", e)))?;

            // Branch deletion failure is not fatal (may not exist)
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // Only log, don't fail
                eprintln!("Warning: Failed to delete branch {}: {}", branch_name, stderr);
            }
        }

        Ok(())
    }

    /// Check if a worktree exists for the given loop.
    pub fn exists(&self, loop_id: &str) -> bool {
        self.path(loop_id).exists()
    }

    /// Get the worktree path for a loop.
    pub fn path(&self, loop_id: &str) -> PathBuf {
        self.base_path.join(loop_id)
    }

    /// List all worktrees managed by this manager.
    pub fn list(&self) -> Result<Vec<String>> {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.repo_root)
            .output()
            .map_err(|e| LooprError::Worktree(format!("Failed to execute git: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(LooprError::Worktree(format!("Failed to list worktrees: {}", stderr)));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut worktrees = Vec::new();

        for line in stdout.lines() {
            if let Some(path_str) = line.strip_prefix("worktree ") {
                let path = Path::new(path_str);
                // Only include worktrees in our base path
                if path.starts_with(&self.base_path)
                    && let Some(name) = path.file_name()
                {
                    worktrees.push(name.to_string_lossy().to_string());
                }
            }
        }

        Ok(worktrees)
    }

    /// Check if a worktree has uncommitted changes.
    pub fn is_clean(&self, loop_id: &str) -> Result<bool> {
        let worktree_path = self.path(loop_id);

        if !worktree_path.exists() {
            return Err(LooprError::Worktree(format!("Worktree does not exist: {}", loop_id)));
        }

        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&worktree_path)
            .output()
            .map_err(|e| LooprError::Worktree(format!("Failed to execute git: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(LooprError::Worktree(format!("Failed to check status: {}", stderr)));
        }

        // Empty output means clean
        Ok(output.stdout.is_empty())
    }

    /// Auto-commit any changes in the worktree.
    ///
    /// Stages all changes and commits with the given message.
    /// Does nothing if the worktree is clean.
    pub fn auto_commit(&self, loop_id: &str, message: &str) -> Result<()> {
        let worktree_path = self.path(loop_id);

        if !worktree_path.exists() {
            return Err(LooprError::Worktree(format!("Worktree does not exist: {}", loop_id)));
        }

        // Check if clean first
        if self.is_clean(loop_id)? {
            return Ok(());
        }

        // Stage all changes
        let output = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&worktree_path)
            .output()
            .map_err(|e| LooprError::Worktree(format!("Failed to execute git add: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(LooprError::Worktree(format!("Failed to stage changes: {}", stderr)));
        }

        // Commit
        let output = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(&worktree_path)
            .output()
            .map_err(|e| LooprError::Worktree(format!("Failed to execute git commit: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(LooprError::Worktree(format!("Failed to commit changes: {}", stderr)));
        }

        Ok(())
    }

    /// Get the branch name for a loop.
    fn branch_name(&self, loop_id: &str) -> String {
        format!("loop/{}", loop_id)
    }

    /// Get the base path for worktrees.
    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// Get the repo root path.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn setup_test_repo() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let repo_path = temp.path().join("repo");
        std::fs::create_dir(&repo_path).unwrap();

        // Initialize git repo
        Command::new("git")
            .args(["init"])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        // Configure git
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        // Create initial commit on main
        std::fs::write(repo_path.join("README.md"), "# Test").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(&repo_path)
            .output()
            .unwrap();

        (temp, repo_path)
    }

    #[test]
    fn test_new() {
        let manager = WorktreeManager::new("/tmp/worktrees", "/tmp/repo");
        assert_eq!(manager.base_path(), Path::new("/tmp/worktrees"));
        assert_eq!(manager.repo_root(), Path::new("/tmp/repo"));
    }

    #[test]
    fn test_path() {
        let manager = WorktreeManager::new("/tmp/worktrees", "/tmp/repo");
        assert_eq!(manager.path("loop-123"), PathBuf::from("/tmp/worktrees/loop-123"));
    }

    #[test]
    fn test_branch_name() {
        let manager = WorktreeManager::new("/tmp/worktrees", "/tmp/repo");
        assert_eq!(manager.branch_name("loop-123"), "loop/loop-123");
    }

    #[test]
    fn test_exists_false() {
        let manager = WorktreeManager::new("/tmp/nonexistent", "/tmp/repo");
        assert!(!manager.exists("loop-123"));
    }

    #[test]
    fn test_create_and_exists() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        // Initially doesn't exist
        assert!(!manager.exists("test-loop"));

        // Create worktree
        let path = manager.create("test-loop").unwrap();
        assert_eq!(path, worktrees_path.join("test-loop"));
        assert!(manager.exists("test-loop"));
        assert!(path.exists());

        // Verify branch was created
        let output = Command::new("git")
            .args(["branch", "--list", "loop/test-loop"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        assert!(!output.stdout.is_empty());
    }

    #[test]
    fn test_cleanup_with_branch_delete() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        // Create and then cleanup
        manager.create("test-loop").unwrap();
        assert!(manager.exists("test-loop"));

        manager.cleanup("test-loop", false).unwrap();
        assert!(!manager.exists("test-loop"));

        // Verify branch was deleted
        let output = Command::new("git")
            .args(["branch", "--list", "loop/test-loop"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        assert!(output.stdout.is_empty());
    }

    #[test]
    fn test_cleanup_preserve_branch() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        // Create and then cleanup with preserve
        manager.create("test-loop").unwrap();
        manager.cleanup("test-loop", true).unwrap();
        assert!(!manager.exists("test-loop"));

        // Verify branch still exists
        let output = Command::new("git")
            .args(["branch", "--list", "loop/test-loop"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        assert!(!output.stdout.is_empty());
    }

    #[test]
    fn test_list_empty() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        let list = manager.list().unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_list_with_worktrees() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        manager.create("loop-1").unwrap();
        manager.create("loop-2").unwrap();

        let list = manager.list().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.contains(&"loop-1".to_string()));
        assert!(list.contains(&"loop-2".to_string()));
    }

    #[test]
    fn test_is_clean_true() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        manager.create("test-loop").unwrap();

        // Should be clean initially
        assert!(manager.is_clean("test-loop").unwrap());
    }

    #[test]
    fn test_is_clean_false() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        let path = manager.create("test-loop").unwrap();

        // Create a new file to make it dirty
        std::fs::write(path.join("new_file.txt"), "content").unwrap();

        assert!(!manager.is_clean("test-loop").unwrap());
    }

    #[test]
    fn test_is_clean_nonexistent() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        let result = manager.is_clean("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_auto_commit_clean() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        manager.create("test-loop").unwrap();

        // Auto-commit on clean repo should succeed (no-op)
        manager.auto_commit("test-loop", "Test commit").unwrap();
        assert!(manager.is_clean("test-loop").unwrap());
    }

    #[test]
    fn test_auto_commit_with_changes() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        let path = manager.create("test-loop").unwrap();

        // Create a new file
        std::fs::write(path.join("new_file.txt"), "content").unwrap();
        assert!(!manager.is_clean("test-loop").unwrap());

        // Auto-commit should commit the changes
        manager.auto_commit("test-loop", "Test commit").unwrap();
        assert!(manager.is_clean("test-loop").unwrap());

        // Verify commit was made
        let output = Command::new("git")
            .args(["log", "--oneline", "-1"])
            .current_dir(&path)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&output.stdout);
        assert!(log.contains("Test commit"));
    }

    #[test]
    fn test_auto_commit_nonexistent() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        let result = manager.auto_commit("nonexistent", "Test commit");
        assert!(result.is_err());
    }

    #[test]
    fn test_create_duplicate_fails() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        // First create succeeds
        manager.create("test-loop").unwrap();

        // Second create should fail
        let result = manager.create("test-loop");
        assert!(result.is_err());
    }

    #[test]
    fn test_cleanup_nonexistent_succeeds() {
        let (temp, repo_path) = setup_test_repo();
        let worktrees_path = temp.path().join("worktrees");
        let manager = WorktreeManager::new(&worktrees_path, &repo_path);

        // Cleanup of nonexistent worktree should succeed (no-op for worktree)
        // Branch deletion may warn but won't fail
        let result = manager.cleanup("nonexistent", false);
        assert!(result.is_ok());
    }
}
