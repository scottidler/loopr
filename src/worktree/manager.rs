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
        Self {
            repo_path,
            worktree_dir,
        }
    }

    /// Create a worktree for a work.
    ///
    /// Runs: `git worktree add <path> -b agent/<work_id> <base_ref>`
    pub fn create(&self, work_id: &str, base_ref: &str) -> Result<PathBuf, WorktreeError> {
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

        Ok(path)
    }

    /// Refresh a worktree to the latest Published Tick's SHA (clears staleness).
    ///
    /// Runs: `git -C <worktree> rebase <new_base_ref>`
    pub fn refresh(&self, work_id: &str, new_base_ref: &str) -> Result<(), WorktreeError> {
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
}
