//! Integration test for scoped staging
//! (docs/design/2026-04-26-scoped-staging.md). Drives a real
//! `Worktree` end-to-end through `dispatch_action` with a Work that
//! has a non-empty `files` scope, and asserts the porcelain partition
//! pipeline lands only in-scope paths in the commit.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use tempfile::TempDir;

use agents::{ActionResult, AgentAction, DispatchError, ToolExecutor, dispatch_action};
use domain::{PlanId, Work};
use worktree::Worktree;

struct NoopTools;

impl ToolExecutor for NoopTools {
    async fn execute(&self, _tool: &str, _input: &serde_json::Value, _wd: &Path) -> Result<String, DispatchError> {
        Ok(String::new())
    }
}

fn init_repo() -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "initial\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    let sha = git_capture(&path, &["rev-parse", "HEAD"]);
    (dir, path, sha)
}

fn git(path: &Path, args: &[&str]) {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

fn git_capture(path: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn scoped_staging_end_to_end_drops_out_of_scope_and_populates_bundle_paths() {
    let (_dir, repo_path, sha) = init_repo();
    let worktree_root = repo_path.parent().unwrap().join("scoped-staging-wts");
    std::fs::create_dir_all(&worktree_root).unwrap();
    let mut work = Work::new(PlanId::new(), "scoped staging e2e".to_string());
    work.files = vec!["main.py".to_string(), "test_api.py".to_string()];
    let wt = Worktree::create(&repo_path, &worktree_root, work.id.clone(), &sha).unwrap();
    let wt_path = wt.path();

    // Implementer creates the in-scope files plus an out-of-scope one.
    std::fs::write(wt_path.join("main.py"), "x = 1\n").unwrap();
    std::fs::write(wt_path.join("test_api.py"), "assert x\n").unwrap();
    std::fs::write(wt_path.join("database.py"), "secret = 'leak'\n").unwrap();

    // Round 1: commit_changes should land main.py and test_api.py and
    // drop database.py with a populated `dropped` field.
    let action1 = AgentAction::CommitChanges {
        message: "implement feature".into(),
    };
    let result1 = dispatch_action(action1, &work, &wt, &NoopTools).await.unwrap();
    match result1 {
        ActionResult::Committed { sha, dropped } => {
            assert_eq!(sha.len(), 40);
            assert_eq!(dropped, vec!["database.py".to_string()]);
        }
        other => panic!("expected Committed, got {other:?}"),
    }
    let names = git_capture(wt_path, &["show", "--name-only", "--format=", "HEAD"]);
    let mut head_names: Vec<&str> = names.lines().filter(|s| !s.is_empty()).collect();
    head_names.sort();
    assert_eq!(head_names, vec!["main.py", "test_api.py"]);

    // Round 2: propose_bundle. database.py is still on disk and dirty;
    // the partition again excludes it. bundle.paths reflects every
    // in-scope path landed across commits, not just the staging step.
    let action2 = AgentAction::ProposeBundle {
        claims: vec!["did the work".into()],
    };
    let result2 = dispatch_action(action2, &work, &wt, &NoopTools).await.unwrap();
    match result2 {
        ActionResult::BundleCreated { bundle, dropped } => {
            assert_eq!(dropped, vec!["database.py".to_string()]);
            let mut paths = bundle.paths.clone();
            paths.sort();
            assert_eq!(paths, vec!["main.py".to_string(), "test_api.py".to_string()]);
        }
        other => panic!("expected BundleCreated, got {other:?}"),
    }

    // database.py never reaches a commit even though it remains dirty
    // in the worktree.
    let log = git_capture(wt_path, &["log", "--all", "--name-only", "--format="]);
    assert!(
        !log.lines().any(|l| l == "database.py"),
        "database.py must NEVER appear in any commit's tree across the branch: {log}"
    );
}
