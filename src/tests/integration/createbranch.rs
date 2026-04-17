#![allow(clippy::unwrap_used)]

//! Phase 4 self-idempotency tests for `CreateIntegrationBranch`.
//!
//! The primitive's only question is "does the branch exist". These tests verify
//! the three wedge scenarios the Architect flagged:
//!   1. Back-to-back invocation.
//!   2. Branch exists but has diverged from main (agent commit).
//!   3. Main has advanced unrelated to the integration branch.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::daemon::context::Stores;
use crate::fsm::runtime::FsmInterpreter;
use crate::ipc::protocol::DaemonEvent;
use crate::primitive::catalog::integration::CreateIntegrationBranch;
use crate::primitive::types::{Idempotency, Primitive, PrimitiveContext};
use crate::test_util::TestDir;
use crate::worktree::manager::WorktreeManager;

/// Initialize a real git repo with a `main` branch and one empty commit. Returns
/// the TestDir and a Stores whose repo_path points into it.
fn test_stores_with_git() -> (TestDir, Arc<Stores>) {
    let dir = TestDir::new("loopr-create-branch-test");
    let path = dir.to_path_buf();
    run_git(&path, &["init", "-b", "main"]);
    run_git(&path, &["commit", "--allow-empty", "-m", "init"]);
    let mut stores = Stores::new();
    stores.config.project.repo_path = path.clone();
    stores.fsm = Arc::new(FsmInterpreter::embedded().unwrap());
    (dir, Arc::new(stores))
}

fn run_git(path: &std::path::Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "user.email=test@test.com", "-c", "user.name=Test"]);
    cmd.args(args).current_dir(path);
    let output = cmd.output().expect("git command failed");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn branch_exists(path: &std::path::Path, branch: &str) -> bool {
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{}", branch)])
        .current_dir(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn branch_sha(path: &std::path::Path, branch: &str) -> String {
    let output = Command::new("git")
        .args(["rev-parse", branch])
        .current_dir(path)
        .output()
        .expect("git rev-parse failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

async fn invoke_create_integration_branch(stores: &Arc<Stores>, plan_id: &str) -> eyre::Result<()> {
    let prim = CreateIntegrationBranch;
    let (tx, _rx) = broadcast::channel::<DaemonEvent>(16);
    let worktree_mgr = WorktreeManager::new(PathBuf::from("/tmp/noop"), PathBuf::from("/tmp/noop-wt"));
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        tx.clone(),
        worktree_mgr.clone(),
        stores.config.clone(),
        stores.fsm.clone(),
    );
    let repo_path = stores.config.project.repo_path.clone();
    let mut strategy_ctx: HashMap<String, serde_json::Value> = HashMap::new();
    let mut ctx = PrimitiveContext {
        stores,
        bridge: &bridge,
        event_tx: &tx,
        repo_path: &repo_path,
        worktree_mgr: &worktree_mgr,
        strategy_ctx: &mut strategy_ctx,
    };
    prim.execute(&mut ctx, json!({ "plan-id": plan_id })).await.map(|_| ())
}

#[test]
fn create_integration_branch_is_idempotent_downgrade() {
    // The primitive must no longer require a strategy-side guard now that it
    // performs its own show-ref check.
    assert_eq!(CreateIntegrationBranch.idempotency(), Idempotency::Idempotent);
}

#[tokio::test]
async fn create_integration_branch_back_to_back_is_safe() {
    let (_dir, stores) = test_stores_with_git();
    let plan_id = "pl-idempo-1";
    let branch = format!("integration/{}", plan_id);

    invoke_create_integration_branch(&stores, plan_id)
        .await
        .expect("first invocation should succeed");
    let sha_after_first = branch_sha(&stores.config.project.repo_path, &branch);

    invoke_create_integration_branch(&stores, plan_id)
        .await
        .expect("second invocation should succeed");
    let sha_after_second = branch_sha(&stores.config.project.repo_path, &branch);

    assert!(branch_exists(&stores.config.project.repo_path, &branch));
    assert_eq!(
        sha_after_first, sha_after_second,
        "branch state must be unchanged between back-to-back invocations"
    );
}

#[tokio::test]
async fn create_integration_branch_tolerates_divergent_branch() {
    // Simulate an agent committing to the integration branch after it was created.
    // The second invocation must return Ok and leave the branch untouched.
    let (_dir, stores) = test_stores_with_git();
    let plan_id = "pl-diverge-1";
    let branch = format!("integration/{}", plan_id);
    let path = stores.config.project.repo_path.clone();

    invoke_create_integration_branch(&stores, plan_id)
        .await
        .expect("create should succeed");

    // Commit on the integration branch to diverge it from main.
    run_git(&path, &["checkout", &branch]);
    run_git(&path, &["commit", "--allow-empty", "-m", "agent work"]);
    let sha_before = branch_sha(&path, &branch);

    run_git(&path, &["checkout", "main"]);

    invoke_create_integration_branch(&stores, plan_id)
        .await
        .expect("re-invoke against divergent branch should still succeed");

    let sha_after = branch_sha(&path, &branch);
    assert_eq!(
        sha_before, sha_after,
        "primitive must not touch an existing integration branch"
    );
}

#[tokio::test]
async fn create_integration_branch_tolerates_advanced_main() {
    // Simulate `main` advancing from an unrelated plan's merge. The primitive
    // must not complain or try to reset the integration branch.
    let (_dir, stores) = test_stores_with_git();
    let plan_id = "pl-advance-1";
    let branch = format!("integration/{}", plan_id);
    let path = stores.config.project.repo_path.clone();

    invoke_create_integration_branch(&stores, plan_id)
        .await
        .expect("create should succeed");
    let sha_before = branch_sha(&path, &branch);

    // Advance main with an unrelated commit.
    run_git(&path, &["commit", "--allow-empty", "-m", "unrelated merge"]);

    invoke_create_integration_branch(&stores, plan_id)
        .await
        .expect("re-invoke after main advance should succeed");

    let sha_after = branch_sha(&path, &branch);
    assert_eq!(
        sha_before, sha_after,
        "primitive must not re-point the integration branch at main"
    );
}
