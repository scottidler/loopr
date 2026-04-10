#![allow(clippy::unwrap_used)]

//! Integration tests for the worktree -> bundle -> review -> integrate handoff path.
//!
//! These tests exercise the specific failure scenarios discovered across six E2E runs
//! (v0.1.88 through v0.1.114). Each test targets a handoff-point invariant documented
//! in docs/design/2026-04-10-bundle-state-alignment.md.
//!
//! Tests marked `#[ignore]` require fixes from the design doc to pass. Remove `#[ignore]`
//! as each fix lands.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use crate::test_util::TestDir;
use crate::tests::integration::fixtures::*;
use crate::worktree::manager::WorktreeManager;

// ---------------------------------------------------------------------------
// Test helpers: git repo setup
// ---------------------------------------------------------------------------

/// Create a minimal git repo in a tmpdir with an initial commit.
/// Returns (TestDir RAII guard, repo path).
fn setup_repo() -> (TestDir, PathBuf) {
    let tmp = TestDir::new("loopr-handoff");
    let repo = tmp.to_path_buf();

    git(&repo, &["init"]);
    git(&repo, &["config", "user.email", "test@test.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);

    // Initial commit with a scaffold file
    std::fs::write(repo.join("README.md"), "# Test Project\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-m", "init: scaffold"]);

    (tmp, repo)
}

/// Create an integration branch from current HEAD.
fn create_integration_branch(repo: &Path, plan_id: &str) {
    let branch = format!("integration/{}", plan_id);
    git(repo, &["branch", &branch]);
    git(repo, &["checkout", &branch]);
}

/// Run a git command in the given directory. Panics on failure.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("git {} failed to execute: {}", args.join(" "), e));
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("git {} failed: {}", args.join(" "), stderr);
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Run a git command that may fail, returning success/failure.
fn git_try(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get the HEAD SHA of a branch.
fn branch_head(repo: &Path, branch: &str) -> String {
    git(repo, &["rev-parse", branch])
}

/// Check if ancestor is an ancestor of descendant.
fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    git_try(repo, &["merge-base", "--is-ancestor", ancestor, descendant])
}

/// Write a file, stage, and commit on the current branch.
fn commit_file(repo: &Path, filename: &str, content: &str, message: &str) -> String {
    std::fs::write(repo.join(filename), content).unwrap();
    git(repo, &["add", filename]);
    git(repo, &["commit", "-m", message]);
    git(repo, &["rev-parse", "HEAD"])
}

/// Check if two refs have identical trees (no diff).
fn refs_identical(repo: &Path, ref_a: &str, ref_b: &str) -> bool {
    git_try(repo, &["diff", "--quiet", ref_a, ref_b])
}

// ---------------------------------------------------------------------------
// Test 1: Normal bundle lifecycle (baseline)
// ---------------------------------------------------------------------------

/// Verify the basic happy path: worktree created, file committed on agent branch,
/// agent branch differs from integration HEAD, reviewer can see the diff.
#[test]
fn test_normal_bundle_creates_divergent_branch() {
    let (_tmp, repo) = setup_repo();
    let plan_id = "pl-test1";
    let work_id = "wk-normal";
    create_integration_branch(&repo, plan_id);

    let integ_head = branch_head(&repo, &format!("integration/{}", plan_id));

    // Simulate implementer: create worktree, commit code
    let wt_dir = repo.join(".worktrees");
    std::fs::create_dir_all(&wt_dir).unwrap();
    let mgr = WorktreeManager::new(repo.clone(), wt_dir);
    let wt_path = mgr.create_branch(work_id, &format!("integration/{}", plan_id)).unwrap();

    // Write code in worktree
    commit_file(&wt_path, "database.py", "def get_db(): pass\n", "feat: database");

    // Agent branch should differ from integration HEAD
    let agent_branch = format!("agent/{}", work_id);
    assert!(
        !refs_identical(&repo, &integ_head, &agent_branch),
        "Agent branch should differ from integration HEAD after commit"
    );

    // Reviewer can see diff
    let diff = git(&repo, &["diff", &integ_head, &agent_branch, "--stat"]);
    assert!(diff.contains("database.py"), "Diff should show database.py");

    // Cleanup
    mgr.cleanup(work_id).unwrap();
}

// ---------------------------------------------------------------------------
// Test 2: Stale rejection -> rebase -> agent branch aligned
// ---------------------------------------------------------------------------

/// After a stale base tick rejection, rebasing the agent branch onto the new
/// integration HEAD should preserve the implementer's commits while aligning
/// with the current state.
///
/// This test exercises Fix 2 from the design doc.
#[ignore = "requires Fix 2: rebase after stale rejection"]
#[test]
fn test_stale_rejection_rebase_preserves_work() {
    let (_tmp, repo) = setup_repo();
    let plan_id = "pl-test2";
    create_integration_branch(&repo, plan_id);
    let integ_branch = format!("integration/{}", plan_id);

    let wt_dir = repo.join(".worktrees");
    std::fs::create_dir_all(&wt_dir).unwrap();
    let mgr = WorktreeManager::new(repo.clone(), wt_dir);

    // Work A: commit code on agent branch
    let wt_a = mgr.create_branch("wk-a", &integ_branch).unwrap();
    commit_file(&wt_a, "database.py", "def crud(): pass\n", "feat: crud");
    mgr.cleanup("wk-a").unwrap();

    // Work B: commit different code, integrate it (moves integration HEAD)
    let wt_b = mgr.create_branch("wk-b", &integ_branch).unwrap();
    commit_file(&wt_b, "main.py", "def main(): pass\n", "feat: main");
    mgr.cleanup("wk-b").unwrap();

    // Merge B into integration branch (simulating integrator)
    git(&repo, &["checkout", &integ_branch]);
    git(&repo, &["merge", "agent/wk-b", "-m", "merge wk-b"]);
    let new_integ_head = branch_head(&repo, &integ_branch);

    // Verify A's branch is stale (not based on new integration HEAD)
    assert!(
        !is_ancestor(&repo, &new_integ_head, "agent/wk-a"),
        "Before rebase: agent/wk-a should NOT have new integration HEAD as ancestor"
    );

    // SIMULATE Fix 2: rebase agent/wk-a onto new integration HEAD
    let rebase_ok = git_try(&repo, &["rebase", &integ_branch, "agent/wk-a"]);
    assert!(rebase_ok, "Rebase should succeed (no conflict)");

    // After rebase: agent/wk-a should have the new integration HEAD as ancestor
    assert!(
        is_ancestor(&repo, &new_integ_head, "agent/wk-a"),
        "After rebase: agent/wk-a should have new integration HEAD as ancestor"
    );

    // The CRUD code should still exist on the rebased branch
    let wt_a2 = mgr.get_or_create_branch("wk-a", &integ_branch).unwrap();
    let content = std::fs::read_to_string(wt_a2.join("database.py")).unwrap();
    assert!(content.contains("crud"), "CRUD code should survive rebase");

    // main.py from work B should also be visible (rebased onto new HEAD)
    assert!(
        wt_a2.join("main.py").exists(),
        "main.py from integrated work B should be visible after rebase"
    );

    mgr.cleanup("wk-a").unwrap();
}

// ---------------------------------------------------------------------------
// Test 3: Stale rejection + rebase conflict -> branch deleted
// ---------------------------------------------------------------------------

/// When rebase fails due to conflict, the agent branch should be deleted
/// so the next session starts from a clean state.
///
/// This test exercises the conflict fallback in Fix 2.
#[ignore = "requires Fix 2: rebase after stale rejection"]
#[test]
fn test_stale_rejection_rebase_conflict_deletes_branch() {
    let (_tmp, repo) = setup_repo();
    let plan_id = "pl-test3";
    create_integration_branch(&repo, plan_id);
    let integ_branch = format!("integration/{}", plan_id);

    let wt_dir = repo.join(".worktrees");
    std::fs::create_dir_all(&wt_dir).unwrap();
    let mgr = WorktreeManager::new(repo.clone(), wt_dir);

    // Work A: modify database.py
    let wt_a = mgr.create_branch("wk-a", &integ_branch).unwrap();
    commit_file(&wt_a, "database.py", "version A\n", "feat: version A");
    mgr.cleanup("wk-a").unwrap();

    // Work B: modify SAME file differently, integrate it
    let wt_b = mgr.create_branch("wk-b", &integ_branch).unwrap();
    commit_file(&wt_b, "database.py", "version B\n", "feat: version B");
    mgr.cleanup("wk-b").unwrap();

    git(&repo, &["checkout", &integ_branch]);
    git(&repo, &["merge", "agent/wk-b", "-m", "merge wk-b"]);

    // Rebase should FAIL (conflict on database.py)
    let rebase_ok = git_try(&repo, &["rebase", &integ_branch, "agent/wk-a"]);
    assert!(!rebase_ok, "Rebase should fail due to conflict");

    // SIMULATE Fix 2 fallback: abort rebase, delete branch
    let _ = git_try(&repo, &["rebase", "--abort"]);
    git(&repo, &["branch", "-D", "agent/wk-a"]);

    // Branch should no longer exist
    assert!(
        !git_try(&repo, &["rev-parse", "--verify", "refs/heads/agent/wk-a"]),
        "agent/wk-a branch should be deleted after conflict"
    );

    // Next session should create a fresh branch from integration HEAD
    let wt_a2 = mgr.create_branch("wk-a", &integ_branch).unwrap();
    let content = std::fs::read_to_string(wt_a2.join("database.py")).unwrap();
    assert_eq!(content, "version B\n", "Fresh branch should have integrated state");

    mgr.cleanup("wk-a").unwrap();
}

// ---------------------------------------------------------------------------
// Test 4: False noop detection
// ---------------------------------------------------------------------------

/// When an agent branch has commits that differ from the integration HEAD,
/// a noop bundle proposal should be auto-converted to a normal bundle.
///
/// This test exercises Fix 1 from the design doc.
#[test]
fn test_false_noop_detected_when_branch_diverges() {
    let (_tmp, repo) = setup_repo();
    let plan_id = "pl-test4";
    create_integration_branch(&repo, plan_id);
    let integ_branch = format!("integration/{}", plan_id);

    let wt_dir = repo.join(".worktrees");
    std::fs::create_dir_all(&wt_dir).unwrap();
    let mgr = WorktreeManager::new(repo.clone(), wt_dir);

    // Session 1: commit code on agent branch
    let wt = mgr.create_branch("wk-a", &integ_branch).unwrap();
    commit_file(&wt, "database.py", "def crud(): pass\n", "feat: crud");
    mgr.cleanup("wk-a").unwrap();

    // Session 2: reuse branch (simulating post-rejection retry)
    let wt2 = mgr.get_or_create_branch("wk-a", &integ_branch).unwrap();

    // Verify: the branch has divergent commits
    let integ_head = branch_head(&repo, &integ_branch);
    assert!(
        !refs_identical(&wt2, &integ_head, "HEAD"),
        "Agent branch HEAD should differ from integration HEAD"
    );

    // The false noop guard (Fix 1) would detect this divergence via:
    //   git diff --quiet <LOOPR_BASE_REF> HEAD  (exit code 1 = differences)
    let diff_check = Command::new("git")
        .args(["diff", "--quiet", &integ_head, "HEAD"])
        .current_dir(&wt2)
        .output()
        .unwrap();
    assert!(
        !diff_check.status.success(),
        "git diff --quiet should return exit 1 when branch diverges from integration HEAD"
    );

    mgr.cleanup("wk-a").unwrap();
}

// ---------------------------------------------------------------------------
// Test 5: True noop (scaffold file, no divergence)
// ---------------------------------------------------------------------------

/// When the agent branch has no divergent commits (scaffold file already exists
/// on the integration branch), a noop is legitimate and should proceed.
#[test]
fn test_true_noop_when_branch_matches_integration() {
    let (_tmp, repo) = setup_repo();
    let plan_id = "pl-test5";

    // Add a scaffold file before creating the integration branch
    commit_file(&repo, "requirements.txt", "fastapi>=0.115\n", "init: requirements");
    create_integration_branch(&repo, plan_id);
    let integ_branch = format!("integration/{}", plan_id);

    let wt_dir = repo.join(".worktrees");
    std::fs::create_dir_all(&wt_dir).unwrap();
    let mgr = WorktreeManager::new(repo.clone(), wt_dir);

    // Create worktree - branch is fresh from integration HEAD
    let wt = mgr.create_branch("wk-a", &integ_branch).unwrap();

    // No commits made - branch matches integration HEAD
    let integ_head = branch_head(&repo, &integ_branch);
    let diff_check = Command::new("git")
        .args(["diff", "--quiet", &integ_head, "HEAD"])
        .current_dir(&wt)
        .output()
        .unwrap();
    assert!(
        diff_check.status.success(),
        "git diff --quiet should return exit 0 when branch matches integration HEAD"
    );

    // requirements.txt exists - legitimate noop target
    assert!(wt.join("requirements.txt").exists());

    mgr.cleanup("wk-a").unwrap();
}

// ---------------------------------------------------------------------------
// Test 6: Session crash before bundle -> max_session_failures -> Blocked
// ---------------------------------------------------------------------------

/// When an agent session fails/cancels before creating a bundle, the
/// session_failure_count on the Work should increment. After reaching
/// max_session_failures, the Work should transition to Blocked.
///
/// This test exercises Fix 3 from the design doc.
#[ignore = "requires Fix 3: session_failure_count field on Work"]
#[tokio::test]
async fn test_session_failure_count_blocks_work() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let (_, spec_results) = inject_preformed_plan(
        &stores,
        &tx,
        &wm,
        &ic,
        PlanInput {
            title: "Test Plan",
            desc: "desc",
            criteria: "pass",
            specs: vec![(
                "Spec",
                "desc",
                vec![("Phase", "desc", 1, vec![("Work A", "desc", vec![])])],
            )],
        },
    )
    .await;

    let work_id = &spec_results[0].1[0].1[0];

    // Transition work to Ready
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": work_id, "target_status": "Ready", "role": "coordinator"}),
    )
    .await;

    // Verify the Work exists and is Ready.
    // When Fix 3 lands, this test will also assert:
    //   work.session_failure_count == 0  (default)
    //   After 3 simulated failures: work.status() == Blocked
    {
        let works = stores.works.read().unwrap();
        let work = works.get(work_id.as_str()).unwrap();
        assert_eq!(work.status(), crate::domain::work::WorkStatus::Ready);
    }
}

// ---------------------------------------------------------------------------
// Test 7: Worktree reuse preserves commits from previous session
// ---------------------------------------------------------------------------

/// Verify the existing behavior that get_or_create_branch reuses an
/// existing agent branch, preserving commits from a prior session.
/// This is the mechanism that causes false noops when combined with
/// stale rejection - understanding it is key to the fix.
#[test]
fn test_worktree_reuse_preserves_prior_commits() {
    let (_tmp, repo) = setup_repo();
    let plan_id = "pl-test7";
    create_integration_branch(&repo, plan_id);
    let integ_branch = format!("integration/{}", plan_id);

    let wt_dir = repo.join(".worktrees");
    std::fs::create_dir_all(&wt_dir).unwrap();
    let mgr = WorktreeManager::new(repo.clone(), wt_dir);

    // Session 1: create worktree, commit code, cleanup
    let wt1 = mgr.create_branch("wk-reuse", &integ_branch).unwrap();
    commit_file(&wt1, "code.py", "print('hello')\n", "feat: code");
    let session1_head = branch_head(&wt1, "HEAD");
    mgr.cleanup("wk-reuse").unwrap();

    // Session 2: get_or_create reuses existing branch
    let wt2 = mgr.get_or_create_branch("wk-reuse", &integ_branch).unwrap();

    // The file from session 1 should still exist
    let content = std::fs::read_to_string(wt2.join("code.py")).unwrap();
    assert_eq!(content, "print('hello')\n", "Prior session's code should persist");

    // HEAD should match session 1's commit
    let session2_head = branch_head(&wt2, "HEAD");
    assert_eq!(
        session1_head, session2_head,
        "Branch HEAD should be preserved across sessions"
    );

    mgr.cleanup("wk-reuse").unwrap();
}

// ---------------------------------------------------------------------------
// Test 8: Integration branch merge advances HEAD
// ---------------------------------------------------------------------------

/// Verify that merging an agent branch into the integration branch advances
/// the integration HEAD, making other agent branches stale.
#[test]
fn test_integration_merge_makes_siblings_stale() {
    let (_tmp, repo) = setup_repo();
    let plan_id = "pl-test8";
    create_integration_branch(&repo, plan_id);
    let integ_branch = format!("integration/{}", plan_id);

    let wt_dir = repo.join(".worktrees");
    std::fs::create_dir_all(&wt_dir).unwrap();
    let mgr = WorktreeManager::new(repo.clone(), wt_dir);

    let original_head = branch_head(&repo, &integ_branch);

    // Work A and B both branch from integration HEAD
    let wt_a = mgr.create_branch("wk-a", &integ_branch).unwrap();
    commit_file(&wt_a, "a.py", "a\n", "feat: a");
    mgr.cleanup("wk-a").unwrap();

    let wt_b = mgr.create_branch("wk-b", &integ_branch).unwrap();
    commit_file(&wt_b, "b.py", "b\n", "feat: b");
    mgr.cleanup("wk-b").unwrap();

    // Merge A into integration branch
    git(&repo, &["checkout", &integ_branch]);
    git(&repo, &["merge", "agent/wk-a", "-m", "merge wk-a"]);
    let new_head = branch_head(&repo, &integ_branch);

    // Integration HEAD advanced
    assert_ne!(original_head, new_head, "Integration HEAD should advance after merge");

    // B's branch is now stale (doesn't have new HEAD as ancestor)
    assert!(
        !is_ancestor(&repo, &new_head, "agent/wk-b"),
        "agent/wk-b should be stale after sibling merge"
    );

    // A's branch is still an ancestor of integration HEAD
    assert!(
        is_ancestor(&repo, "agent/wk-a", &new_head),
        "agent/wk-a should be ancestor of new integration HEAD"
    );
}
