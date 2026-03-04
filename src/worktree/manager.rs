use log::debug;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Information about an active Git worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    pub head: String,
}

/// Manages Git worktrees for work isolation.
///
/// Each work gets its own worktree under `worktree_dir`, allowing
/// implementers to work in isolated branches.
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    pub repo_path: PathBuf,
    pub worktree_dir: PathBuf,
}

/// Errors specific to worktree operations.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("git command failed: {0}")]
    GitCommand(String),

    #[error("worktree not found for work: {0}")]
    NotFound(String),

    #[error("worktree already exists for work: {0}")]
    AlreadyExists(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl WorktreeManager {
    /// Create a new WorktreeManager.
    pub fn new(repo_path: PathBuf, worktree_dir: PathBuf) -> Self {
        debug!(
            "WorktreeManager::new(repo_root={}, worktree_dir={})",
            repo_path.display(),
            worktree_dir.display()
        );
        Self {
            repo_path,
            worktree_dir,
        }
    }

    /// Create a worktree for a work.
    ///
    /// Runs: `git worktree add <path> -b agent/<work_id> <base_ref>`
    pub fn create(&self, work_id: &str, base_ref: &str) -> Result<PathBuf, WorktreeError> {
        debug!("WorktreeManager::create(key={}, base_ref={})", work_id, base_ref);
        let path = self.worktree_dir.join(work_id);
        if path.exists() {
            return Err(WorktreeError::AlreadyExists(work_id.to_string()));
        }

        let branch = format!("agent/{}", work_id);

        // Delete stale branch from a previous failed run if it exists
        let _ = Command::new("git")
            .args(["branch", "-D", &branch])
            .current_dir(&self.repo_path)
            .output();

        let output = Command::new("git")
            .args(["worktree", "add", &path.to_string_lossy(), "-b", &branch, base_ref])
            .current_dir(&self.repo_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::GitCommand(stderr.to_string()));
        }

        // Verify the branch was actually checked out
        let verify = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&path)
            .output()?;
        let actual_branch = String::from_utf8_lossy(&verify.stdout).trim().to_string();
        if actual_branch != branch {
            return Err(WorktreeError::GitCommand(format!(
                "worktree created but branch mismatch: expected '{}', got '{}'",
                branch, actual_branch
            )));
        }

        Ok(path)
    }

    /// Idempotent worktree creation — returns the existing worktree path if one
    /// exists, or creates a new one. Handles TOCTOU races between concurrent agents.
    pub fn get_or_create(&self, work_id: &str, base_ref: &str) -> Result<PathBuf, WorktreeError> {
        debug!("WorktreeManager::get_or_create(key={}, base_ref={})", work_id, base_ref);
        let path = self.worktree_dir.join(work_id);
        if path.exists() {
            // Verify it's a valid git worktree by checking for .git file
            let git_file = path.join(".git");
            if git_file.exists() {
                return Ok(path);
            }
            // Directory exists but isn't a worktree — clean up and recreate
            std::fs::remove_dir_all(&path)?;
        }
        // create() may fail with GitCommand if the branch "agent/<work_id>" already
        // exists (TOCTOU race with another agent). If the path now exists after the
        // failed create (the other agent won), just return it.
        match self.create(work_id, base_ref) {
            Ok(p) => Ok(p),
            Err(WorktreeError::AlreadyExists(_)) => Ok(path),
            Err(e) => {
                // Check if the other racer won and the path now exists
                if path.join(".git").exists() { Ok(path) } else { Err(e) }
            }
        }
    }

    /// Refresh a worktree to the latest Published Tick's SHA (clears staleness).
    ///
    /// Runs: `git -C <worktree> rebase <new_base_ref>`
    pub fn refresh(&self, work_id: &str, new_base_ref: &str) -> Result<(), WorktreeError> {
        debug!("WorktreeManager::refresh(key={}, new_ref={})", work_id, new_base_ref);
        let path = self.worktree_dir.join(work_id);
        if !path.exists() {
            return Err(WorktreeError::NotFound(work_id.to_string()));
        }

        let output = Command::new("git")
            .args(["-C", &path.to_string_lossy(), "rebase", new_base_ref])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::GitCommand(stderr.to_string()));
        }

        Ok(())
    }

    /// Clean up a worktree after bundle is merged or abandoned.
    ///
    /// Runs: `git worktree remove <path>`
    pub fn cleanup(&self, work_id: &str) -> Result<(), WorktreeError> {
        debug!("WorktreeManager::cleanup(key={})", work_id);
        let path = self.worktree_dir.join(work_id);
        if !path.exists() {
            return Err(WorktreeError::NotFound(work_id.to_string()));
        }

        // Use --force to handle worktrees with uncommitted changes
        let output = Command::new("git")
            .args(["worktree", "remove", "--force", &path.to_string_lossy()])
            .current_dir(&self.repo_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::GitCommand(stderr.to_string()));
        }

        // Keep the agent branch alive — the Integrator needs it for merging.
        // Branch cleanup happens after Tick publishes.

        Ok(())
    }

    /// List active worktrees managed by this WorktreeManager.
    ///
    /// Parses output of `git worktree list --porcelain`.
    pub fn list(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.repo_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::GitCommand(stderr.to_string()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let worktrees = parse_worktree_list(&stdout, &self.worktree_dir);
        Ok(worktrees)
    }

    /// Get the worktree path for a work.
    pub fn worktree_path(&self, work_id: &str) -> PathBuf {
        self.worktree_dir.join(work_id)
    }

    /// Check if a worktree exists for a work.
    pub fn exists(&self, work_id: &str) -> bool {
        self.worktree_dir.join(work_id).exists()
    }
}

/// Parse `git worktree list --porcelain` output, filtering to worktrees
/// under the managed worktree directory.
fn parse_worktree_list(output: &str, worktree_dir: &Path) -> Vec<WorktreeInfo> {
    let mut result = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_head = String::new();
    let mut current_branch = String::new();

    for line in output.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            // Flush previous entry
            if let Some(path) = current_path.take()
                && path.starts_with(worktree_dir)
            {
                result.push(WorktreeInfo {
                    path,
                    branch: std::mem::take(&mut current_branch),
                    head: std::mem::take(&mut current_head),
                });
            }
            current_path = Some(PathBuf::from(path_str));
            current_head.clear();
            current_branch.clear();
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            current_head = head.to_string();
        } else if let Some(branch) = line.strip_prefix("branch ") {
            // branch refs/heads/agent/wi-xxx → agent/wi-xxx
            current_branch = branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string();
        }
    }

    // Flush last entry
    if let Some(path) = current_path
        && path.starts_with(worktree_dir)
    {
        result.push(WorktreeInfo {
            path,
            branch: current_branch,
            head: current_head,
        });
    }

    result
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worktree_manager_new() {
        let mgr = WorktreeManager::new(PathBuf::from("/repo"), PathBuf::from("/repo/.worktrees"));
        assert_eq!(mgr.repo_path, PathBuf::from("/repo"));
        assert_eq!(mgr.worktree_dir, PathBuf::from("/repo/.worktrees"));
    }

    #[test]
    fn test_worktree_path() {
        let mgr = WorktreeManager::new(PathBuf::from("/repo"), PathBuf::from("/repo/.worktrees"));
        assert_eq!(mgr.worktree_path("wi-001"), PathBuf::from("/repo/.worktrees/wi-001"));
    }

    #[test]
    fn test_exists_false_for_nonexistent() {
        let mgr = WorktreeManager::new(
            PathBuf::from("/nonexistent/repo"),
            PathBuf::from("/nonexistent/worktrees"),
        );
        assert!(!mgr.exists("wi-001"));
    }

    #[test]
    fn test_parse_worktree_list_empty() {
        let result = parse_worktree_list("", &PathBuf::from("/repo/.worktrees"));
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_worktree_list_filters_by_dir() {
        let output = "\
worktree /repo
HEAD abc123
branch refs/heads/main

worktree /repo/.worktrees/wi-001
HEAD def456
branch refs/heads/agent/wi-001

worktree /other/path
HEAD 789abc
branch refs/heads/feature/x
";
        let result = parse_worktree_list(output, &PathBuf::from("/repo/.worktrees"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/repo/.worktrees/wi-001"));
        assert_eq!(result[0].branch, "agent/wi-001");
        assert_eq!(result[0].head, "def456");
    }

    #[test]
    fn test_parse_worktree_list_multiple() {
        let output = "\
worktree /repo/.worktrees/wi-001
HEAD aaa111
branch refs/heads/agent/wi-001

worktree /repo/.worktrees/wi-002
HEAD bbb222
branch refs/heads/agent/wi-002
";
        let result = parse_worktree_list(output, &PathBuf::from("/repo/.worktrees"));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].head, "aaa111");
        assert_eq!(result[1].head, "bbb222");
    }

    #[test]
    fn test_parse_worktree_list_strips_refs_heads() {
        let output = "\
worktree /wt/wi-001
HEAD abc
branch refs/heads/agent/wi-001
";
        let result = parse_worktree_list(output, &PathBuf::from("/wt"));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].branch, "agent/wi-001");
    }

    #[test]
    fn test_worktree_error_display() {
        let err = WorktreeError::GitCommand("fatal: something".to_string());
        assert_eq!(err.to_string(), "git command failed: fatal: something");

        let err = WorktreeError::NotFound("wi-001".to_string());
        assert_eq!(err.to_string(), "worktree not found for work: wi-001");

        let err = WorktreeError::AlreadyExists("wi-001".to_string());
        assert_eq!(err.to_string(), "worktree already exists for work: wi-001");
    }

    #[test]
    fn test_worktree_info_serde_roundtrip() {
        let info = WorktreeInfo {
            path: PathBuf::from("/repo/.worktrees/wi-001"),
            branch: "agent/wi-001".to_string(),
            head: "abc123".to_string(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let deserialized: WorktreeInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.path, info.path);
        assert_eq!(deserialized.branch, info.branch);
        assert_eq!(deserialized.head, info.head);
    }

    #[test]
    fn test_create_rejects_existing_path() {
        // Use a temp dir that exists to trigger AlreadyExists
        let temp = std::env::temp_dir();
        let worktree_dir = temp.clone();
        let mgr = WorktreeManager::new(temp, worktree_dir);
        // "." exists as a subdirectory name won't work, use the temp dir itself
        // Create a directory that exists
        let test_id = "worktree-test-exists";
        let test_path = mgr.worktree_dir.join(test_id);
        std::fs::create_dir_all(&test_path).ok();
        let result = mgr.create(test_id, "HEAD");
        assert!(matches!(result, Err(WorktreeError::AlreadyExists(_))));
        std::fs::remove_dir(&test_path).ok();
    }

    #[test]
    fn test_refresh_rejects_missing_worktree() {
        let mgr = WorktreeManager::new(PathBuf::from("/nonexistent"), PathBuf::from("/nonexistent/wt"));
        let result = mgr.refresh("wi-missing", "HEAD");
        assert!(matches!(result, Err(WorktreeError::NotFound(_))));
    }

    #[test]
    fn test_cleanup_rejects_missing_worktree() {
        let mgr = WorktreeManager::new(PathBuf::from("/nonexistent"), PathBuf::from("/nonexistent/wt"));
        let result = mgr.cleanup("wi-missing");
        assert!(matches!(result, Err(WorktreeError::NotFound(_))));
    }

    #[test]
    fn test_get_or_create_returns_existing_valid_worktree() {
        // Simulate a valid worktree: directory with a .git file inside
        let temp = std::env::temp_dir().join("loopr-test-get-or-create-valid");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let worktree_dir = temp.join("worktrees");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        let wt_path = worktree_dir.join("wi-existing");
        std::fs::create_dir_all(&wt_path).unwrap();
        // Create a .git file to simulate a valid worktree
        std::fs::write(wt_path.join(".git"), "gitdir: /repo/.git/worktrees/wi-existing").unwrap();

        let mgr = WorktreeManager::new(temp.clone(), worktree_dir);
        let result = mgr.get_or_create("wi-existing", "HEAD");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), wt_path);

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_get_or_create_cleans_invalid_dir_without_git_file() {
        // Directory exists but has no .git file — get_or_create should remove it
        // and attempt create (which will fail since we're not in a real repo,
        // but the cleanup logic is what we're testing)
        let temp = std::env::temp_dir().join("loopr-test-get-or-create-invalid");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let worktree_dir = temp.join("worktrees");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        let wt_path = worktree_dir.join("wi-invalid");
        std::fs::create_dir_all(&wt_path).unwrap();
        // No .git file — should be cleaned up

        let mgr = WorktreeManager::new(temp.clone(), worktree_dir);
        let result = mgr.get_or_create("wi-invalid", "HEAD");
        // create() will fail (not a real git repo), but the invalid dir should be removed
        assert!(result.is_err());
        assert!(
            !wt_path.exists(),
            "invalid dir should have been removed before create attempt"
        );

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_get_or_create_absent_dir_attempts_create() {
        // When no directory exists, get_or_create should attempt create
        let mgr = WorktreeManager::new(
            PathBuf::from("/nonexistent/repo"),
            PathBuf::from("/nonexistent/worktrees"),
        );
        let result = mgr.get_or_create("wi-new", "HEAD");
        // Will fail because /nonexistent/repo isn't a real git repo
        assert!(result.is_err());
    }
}
