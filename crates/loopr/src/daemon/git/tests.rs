use std::process::Command as SyncCommand;

use tempfile::TempDir;

use domain::PlanId;

use super::ensure_integration_branch;

/// Initialize a git repo at `path` with one commit so HEAD exists.
fn init_repo_with_commit(path: &std::path::Path) {
    let run = |args: &[&str]| {
        let out = SyncCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .expect("git subprocess");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    run(&["config", "commit.gpgsign", "false"]);
    run(&["config", "tag.gpgsign", "false"]);
    run(&["commit", "--allow-empty", "-q", "-m", "initial"]);
}

fn branch_exists(target: &std::path::Path, branch: &str) -> bool {
    SyncCommand::new("git")
        .arg("-C")
        .arg(target)
        .args(["rev-parse", "--verify", "--quiet", branch])
        .status()
        .expect("git rev-parse")
        .success()
}

#[tokio::test]
async fn creates_branch_from_head() {
    let dir = TempDir::new().expect("tempdir");
    init_repo_with_commit(dir.path());
    let plan_id = PlanId::new();
    let branch = format!("loopr/plan-{plan_id}");

    assert!(!branch_exists(dir.path(), &branch), "precondition: no branch");
    ensure_integration_branch(dir.path(), &plan_id)
        .await
        .expect("create branch");
    assert!(branch_exists(dir.path(), &branch), "branch now exists");
}

#[tokio::test]
async fn idempotent_second_call_is_noop() {
    let dir = TempDir::new().expect("tempdir");
    init_repo_with_commit(dir.path());
    let plan_id = PlanId::new();
    let branch = format!("loopr/plan-{plan_id}");

    ensure_integration_branch(dir.path(), &plan_id)
        .await
        .expect("first call");
    ensure_integration_branch(dir.path(), &plan_id)
        .await
        .expect("second call is Ok");
    assert!(branch_exists(dir.path(), &branch));
}

#[tokio::test]
async fn fresh_repo_without_head_returns_err() {
    let dir = TempDir::new().expect("tempdir");
    // git init but no commit: HEAD is unborn.
    let out = SyncCommand::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "-q", "-b", "main"])
        .output()
        .expect("git init");
    assert!(out.status.success());

    let plan_id = PlanId::new();
    let err = ensure_integration_branch(dir.path(), &plan_id)
        .await
        .expect_err("unborn HEAD should fail");
    let msg = err.to_string();
    assert!(
        msg.contains("git branch") || msg.contains("HEAD"),
        "error references git: {msg}"
    );
}
