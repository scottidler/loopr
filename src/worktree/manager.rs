use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, warn};

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
    /// If the branch `agent/<work_id>` already exists (from a previous implementer
    /// session), the worktree is created on the existing branch, preserving its
    /// commits. Otherwise a fresh branch is created from `base_ref`.
    pub fn create_branch(&self, work_id: &str, base_ref: &str) -> Result<PathBuf, WorktreeError> {
        debug!("WorktreeManager::create(key={}, base_ref={})", work_id, base_ref);
        let path = self.worktree_dir.join(work_id);
        if path.exists() {
            return Err(WorktreeError::AlreadyExists(work_id.to_string()));
        }

        let branch = format!("agent/{}", work_id);

        // Check if the branch already exists (from a previous implementer session).
        // If so, reuse it to preserve commits that haven't been integrated yet.
        let branch_exists = Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{}", branch)])
            .current_dir(&self.repo_path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        let output = if branch_exists {
            debug!("branch {} exists, creating worktree on existing branch", branch);
            let out = Command::new("git")
                .args(["worktree", "add", &path.to_string_lossy(), &branch])
                .current_dir(&self.repo_path)
                .output()?;
            if out.status.success() {
                // Rebase existing branch onto new base_ref so retried
                // implementers see sibling work merged since the last attempt.
                if let Err(e) = self.refresh(work_id, base_ref) {
                    warn!(
                        "rebase failed for {} - resetting branch to {}: {}",
                        work_id, base_ref, e
                    );
                    let reset = Command::new("git")
                        .args(["-C", &path.to_string_lossy(), "reset", "--hard", base_ref])
                        .output();
                    if let Err(re) = reset {
                        warn!("branch reset also failed for {}: {}", work_id, re);
                    }
                }
            }
            out
        } else {
            Command::new("git")
                .args(["worktree", "add", &path.to_string_lossy(), "-b", &branch, base_ref])
                .current_dir(&self.repo_path)
                .output()?
        };

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
    pub fn get_or_create_branch(&self, work_id: &str, base_ref: &str) -> Result<PathBuf, WorktreeError> {
        debug!(
            "WorktreeManager::get_or_create_branch(key={}, base_ref={})",
            work_id, base_ref
        );
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
        // Prune stale worktree registrations before creating.
        // If a previous agent session for this work_id crashed without cleanup, git
        // still has the worktree registered even though the directory is gone. That
        // causes `git worktree add` to fail with "missing but already registered".
        // `git worktree prune` removes any registrations whose directories no longer exist.
        let prune_out = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.repo_path)
            .output();
        if let Ok(o) = &prune_out
            && !o.status.success()
        {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("git worktree prune failed (non-fatal): {}", stderr.trim());
        }

        // create() may fail with GitCommand if the branch "agent/<work_id>" already
        // exists (TOCTOU race with another agent). If the path now exists after the
        // failed create (the other agent won), just return it.
        match self.create_branch(work_id, base_ref) {
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
            // Abort the half-rebased state so subsequent git operations aren't blocked.
            let _ = Command::new("git")
                .args(["-C", &path.to_string_lossy(), "rebase", "--abort"])
                .output();
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

    /// Delete the agent branch for a work item. Called by the Integrator
    /// after a Tick is published (commits are safely on main), or by the
    /// Coordinator when work is abandoned.
    pub fn delete_branch(&self, work_id: &str) -> Result<(), WorktreeError> {
        let branch = format!("agent/{}", work_id);
        debug!("WorktreeManager::delete_branch(branch={})", branch);
        let output = Command::new("git")
            .args(["branch", "-D", &branch])
            .current_dir(&self.repo_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Branch may already be deleted - not an error
            if !stderr.contains("not found") {
                return Err(WorktreeError::GitCommand(stderr.to_string()));
            }
        }
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

const LOOPR_EXCLUDE_MARKER: &str = "# loopr-managed";
const LOOPR_EXCLUDES: &[&str] = &[".taskstore/", ".worktrees/", "loopr.yml"];

/// Ensure Loopr orchestration artifacts are in .git/info/exclude for the
/// given repository root. Idempotent: checks for marker before appending.
///
/// Because worktrees inherit the common git directory's exclude rules,
/// calling this once on the root repo covers all worktrees.
pub fn ensure_loopr_excludes(repo_path: &Path) -> Result<(), std::io::Error> {
    let exclude_path = repo_path.join(".git").join("info").join("exclude");

    // Read existing content (file may not exist yet)
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.contains(LOOPR_EXCLUDE_MARKER) {
        return Ok(()); // Already injected
    }

    // Ensure .git/info/ directory exists
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Append our patterns
    let mut content = existing;
    if !content.ends_with('\n') && !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&format!("{}\n", LOOPR_EXCLUDE_MARKER));
    for pattern in LOOPR_EXCLUDES {
        content.push_str(&format!("{}\n", pattern));
    }
    std::fs::write(&exclude_path, content)?;
    Ok(())
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
        let result = mgr.create_branch(test_id, "HEAD");
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
    fn test_get_or_create_branch_returns_existing_valid_worktree() {
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
        let result = mgr.get_or_create_branch("wi-existing", "HEAD");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), wt_path);

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_get_or_create_branch_cleans_invalid_dir_without_git_file() {
        // Directory exists but has no .git file — get_or_create_branch should remove it
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
        let result = mgr.get_or_create_branch("wi-invalid", "HEAD");
        // create() will fail (not a real git repo), but the invalid dir should be removed
        assert!(result.is_err());
        assert!(
            !wt_path.exists(),
            "invalid dir should have been removed before create attempt"
        );

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_get_or_create_branch_absent_dir_attempts_create() {
        // When no directory exists, get_or_create_branch should attempt create
        let mgr = WorktreeManager::new(
            PathBuf::from("/nonexistent/repo"),
            PathBuf::from("/nonexistent/worktrees"),
        );
        let result = mgr.get_or_create_branch("wi-new", "HEAD");
        // Will fail because /nonexistent/repo isn't a real git repo
        assert!(result.is_err());
    }

    /// Helper: create a temporary git repo with an initial commit.
    fn init_test_repo(name: &str) -> PathBuf {
        let temp = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let run = |args: &[&str]| {
            Command::new("git").args(args).current_dir(&temp).output().unwrap();
        };
        run(&["init"]);
        run(&["config", "user.email", "test@test.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(temp.join("README.md"), "init").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "init"]);
        temp
    }

    #[test]
    fn test_create_preserves_existing_branch_commits() {
        let repo = init_test_repo("loopr-wt-preserve-branch");
        let wt_dir = repo.join(".worktrees");
        std::fs::create_dir_all(&wt_dir).unwrap();
        let mgr = WorktreeManager::new(repo.clone(), wt_dir);

        // Create first worktree and commit a file
        let path1 = mgr.create_branch("wi-001", "HEAD").unwrap();
        std::fs::write(path1.join("hello.txt"), "world").unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&path1)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add hello.txt"])
            .current_dir(&path1)
            .output()
            .unwrap();

        // Record the commit SHA
        let sha_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&path1)
            .output()
            .unwrap();
        let commit_sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();

        // Cleanup the worktree (simulates implementer finishing)
        mgr.cleanup("wi-001").unwrap();

        // Verify branch still exists with the commit
        let branch_sha = Command::new("git")
            .args(["rev-parse", "agent/wi-001"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let branch_sha = String::from_utf8_lossy(&branch_sha.stdout).trim().to_string();
        assert_eq!(commit_sha, branch_sha, "branch should retain the commit after cleanup");

        // Create second worktree on same work (simulates retry)
        let path2 = mgr.create_branch("wi-001", "HEAD").unwrap();

        // Verify the file from the first session is still there
        assert!(
            path2.join("hello.txt").exists(),
            "hello.txt from first session should persist on the reused branch"
        );

        // Cleanup
        mgr.cleanup("wi-001").unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn test_create_fresh_branch_when_none_exists() {
        let repo = init_test_repo("loopr-wt-fresh-branch");
        let wt_dir = repo.join(".worktrees");
        std::fs::create_dir_all(&wt_dir).unwrap();
        let mgr = WorktreeManager::new(repo.clone(), wt_dir);

        // No branch exists yet - should create fresh
        let path = mgr.create_branch("wi-002", "HEAD").unwrap();
        assert!(path.exists());

        // Verify the branch was created
        let branch_check = Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/agent/wi-002"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(branch_check.status.success());

        mgr.cleanup("wi-002").unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn test_delete_branch_removes_branch() {
        let repo = init_test_repo("loopr-wt-delete-branch");
        let wt_dir = repo.join(".worktrees");
        std::fs::create_dir_all(&wt_dir).unwrap();
        let mgr = WorktreeManager::new(repo.clone(), wt_dir);

        // Create and cleanup a worktree (leaves branch alive)
        let path = mgr.create_branch("wi-003", "HEAD").unwrap();
        std::fs::write(path.join("test.txt"), "data").unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add test.txt"])
            .current_dir(&path)
            .output()
            .unwrap();
        mgr.cleanup("wi-003").unwrap();

        // Branch should exist
        let check = Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/agent/wi-003"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(check.status.success(), "branch should exist before delete");

        // Delete it
        mgr.delete_branch("wi-003").unwrap();

        // Branch should be gone
        let check = Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/agent/wi-003"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(!check.status.success(), "branch should be gone after delete");

        let _ = std::fs::remove_dir_all(&repo);
    }

    // --- ensure_loopr_excludes tests ---

    #[test]
    fn test_ensure_excludes_creates_file_when_missing() {
        let temp = std::env::temp_dir().join("loopr-test-excludes-create");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(".git")).unwrap();

        ensure_loopr_excludes(&temp).unwrap();

        let content = std::fs::read_to_string(temp.join(".git/info/exclude")).unwrap();
        assert!(content.contains(LOOPR_EXCLUDE_MARKER));
        assert!(content.contains(".taskstore/"));
        assert!(content.contains(".worktrees/"));
        assert!(content.contains("loopr.yml"));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_ensure_excludes_idempotent() {
        let temp = std::env::temp_dir().join("loopr-test-excludes-idempotent");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(".git")).unwrap();

        ensure_loopr_excludes(&temp).unwrap();
        let content1 = std::fs::read_to_string(temp.join(".git/info/exclude")).unwrap();

        ensure_loopr_excludes(&temp).unwrap();
        let content2 = std::fs::read_to_string(temp.join(".git/info/exclude")).unwrap();

        assert_eq!(content1, content2, "second call should not append again");

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_ensure_excludes_appends_to_existing() {
        let temp = std::env::temp_dir().join("loopr-test-excludes-append");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(".git/info")).unwrap();
        std::fs::write(temp.join(".git/info/exclude"), "# existing pattern\n*.log\n").unwrap();

        ensure_loopr_excludes(&temp).unwrap();

        let content = std::fs::read_to_string(temp.join(".git/info/exclude")).unwrap();
        assert!(content.starts_with("# existing pattern\n*.log\n"));
        assert!(content.contains(LOOPR_EXCLUDE_MARKER));
        assert!(content.contains(".taskstore/"));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_ensure_excludes_creates_info_dir() {
        let temp = std::env::temp_dir().join("loopr-test-excludes-mkdir");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(".git")).unwrap();
        // .git/info/ does not exist yet

        ensure_loopr_excludes(&temp).unwrap();

        assert!(temp.join(".git/info/exclude").exists());

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_delete_branch_idempotent_on_missing() {
        let repo = init_test_repo("loopr-wt-delete-missing");
        let wt_dir = repo.join(".worktrees");
        std::fs::create_dir_all(&wt_dir).unwrap();
        let mgr = WorktreeManager::new(repo.clone(), wt_dir);

        // Deleting a branch that doesn't exist should succeed
        let result = mgr.delete_branch("wi-nonexistent");
        assert!(result.is_ok(), "deleting missing branch should be Ok");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn test_create_rebases_existing_branch_onto_new_base() {
        let repo = init_test_repo("loopr-wt-rebase-on-retry");
        let wt_dir = repo.join(".worktrees");
        std::fs::create_dir_all(&wt_dir).unwrap();
        let mgr = WorktreeManager::new(repo.clone(), wt_dir);

        let run = |args: &[&str], dir: &Path| {
            Command::new("git").args(args).current_dir(dir).output().unwrap();
        };

        // First attempt: create worktree and commit a file
        let path1 = mgr.create_branch("wi-001", "HEAD").unwrap();
        std::fs::write(path1.join("hello.txt"), "world").unwrap();
        run(&["add", "-A"], &path1);
        run(&["commit", "-m", "add hello.txt"], &path1);

        // Cleanup worktree (simulates session end)
        mgr.cleanup("wi-001").unwrap();

        // Simulate sibling work merged into main (the integration branch tip)
        std::fs::write(repo.join("sibling.txt"), "sibling content").unwrap();
        run(&["add", "-A"], &repo);
        run(&["commit", "-m", "add sibling.txt"], &repo);

        // Retry: create worktree again with base_ref = main (not "HEAD" -
        // refresh runs git -C <worktree> where HEAD would be the agent branch)
        let path2 = mgr.create_branch("wi-001", "main").unwrap();

        // Rebase should have brought sibling.txt into the agent branch
        assert!(
            path2.join("sibling.txt").exists(),
            "sibling.txt should exist after rebase onto new base"
        );
        // Previous implementer commits should be preserved
        assert!(
            path2.join("hello.txt").exists(),
            "hello.txt from first session should survive rebase"
        );

        mgr.cleanup("wi-001").unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn test_create_rebase_conflict_resets_to_base() {
        let repo = init_test_repo("loopr-wt-rebase-conflict-reset");
        let wt_dir = repo.join(".worktrees");
        std::fs::create_dir_all(&wt_dir).unwrap();
        let mgr = WorktreeManager::new(repo.clone(), wt_dir);

        let run = |args: &[&str], dir: &Path| {
            Command::new("git").args(args).current_dir(dir).output().unwrap();
        };

        // First attempt: create worktree, modify README.md and add hello.txt
        let path1 = mgr.create_branch("wi-001", "HEAD").unwrap();
        std::fs::write(path1.join("README.md"), "agent version").unwrap();
        std::fs::write(path1.join("hello.txt"), "agent file").unwrap();
        run(&["add", "-A"], &path1);
        run(&["commit", "-m", "agent changes"], &path1);

        mgr.cleanup("wi-001").unwrap();

        // On main, modify README.md differently (creates conflict)
        std::fs::write(repo.join("README.md"), "main version").unwrap();
        run(&["add", "-A"], &repo);
        run(&["commit", "-m", "main changes to README"], &repo);

        // Retry: rebase will conflict on README.md, should fall back to reset
        let path2 = mgr.create_branch("wi-001", "main").unwrap();

        // After reset to base_ref, README.md should have main's content
        let readme = std::fs::read_to_string(path2.join("README.md")).unwrap();
        assert_eq!(
            readme, "main version",
            "README.md should have main's content after reset fallback"
        );
        // hello.txt from the agent's rejected attempt should be gone
        assert!(
            !path2.join("hello.txt").exists(),
            "hello.txt should be gone after reset to base_ref"
        );

        mgr.cleanup("wi-001").unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }
}
