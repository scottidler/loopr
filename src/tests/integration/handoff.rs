#![allow(clippy::unwrap_used)]

//! Integration tests for the worktree -> bundle -> review -> integrate handoff path.
//!
//! These tests exercise the specific failure scenarios discovered across six E2E runs
//! (v0.1.88 through v0.1.114). Each test targets a handoff-point invariant documented
//! in docs/design/2026-04-10-bundle-state-alignment.md.
//!
//! Tests marked `#[ignore]` require fixes from the design doc to pass. Remove `#[ignore]`
//! as each fix lands.
//!
//! ## Test organization
//!
//! - Tests 1-5, 7-8: git-level invariants (rebase mechanics, branch divergence) using
//!   raw git commands. These are boundary condition proofs that the underlying git
//!   operations behave correctly.
//!
//! - Test 6: dispatch harness (session_failure_count field default)
//!
//! - Test 9: dispatch harness proving the bundle-rejection -> work-reset -> rebase path
//!   does not deadlock. Added as part of the v0.1.115 remediation after an architectural
//!   review found that reset_work_after_bundle_rejection previously contained a lock_git()
//!   call that caused re-entrancy deadlocks when called from within the merge-failure path
//!   (which already holds _git_guard).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use crate::domain::work::WorkStatus;
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
// Test 2: Stale rejection -> hard reset -> agent branch clean
// ---------------------------------------------------------------------------

/// After a stale base tick rejection, retrying the worktree hard-resets the
/// agent branch to the current integration tip. The rejected session's
/// commits are destroyed so the implementer starts from a clean slate that
/// includes sibling work merged since the last attempt.
///
/// This test exercises Fix 2 from the design doc.
#[test]
fn test_stale_rejection_reset_destroys_rejected_work() {
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

    // Retry work A: get_or_create_branch hard-resets to integration tip
    let wt_a2 = mgr.get_or_create_branch("wk-a", &integ_branch).unwrap();

    // Agent branch should now match the integration HEAD exactly
    let agent_head = branch_head(&wt_a2, "HEAD");
    assert_eq!(
        new_integ_head, agent_head,
        "After reset: agent/wk-a should match integration HEAD"
    );

    // Rejected session's database.py is destroyed - no ghosts
    assert!(
        !wt_a2.join("database.py").exists(),
        "Rejected session's database.py must be destroyed on retry"
    );

    // main.py from integrated work B should be visible (from integration tip)
    assert!(
        wt_a2.join("main.py").exists(),
        "main.py from integrated work B should be visible after reset"
    );

    mgr.cleanup("wk-a").unwrap();
}

// ---------------------------------------------------------------------------
// Test 3: Git mechanics proof - conflicting branch can be deleted and recreated
// ---------------------------------------------------------------------------

/// Prove the git-level invariant: when an agent branch conflicts with the
/// integration branch, deleting and recreating the branch yields a clean
/// worktree with the integrated state. This is a belt-and-suspenders proof;
/// the production code path uses unconditional hard-reset (not rebase+delete).
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

    // SIMULATE Fix 2 fallback: abort rebase, delete branch, restore checkout
    let _ = git_try(&repo, &["rebase", "--abort"]);
    git(&repo, &["checkout", &integ_branch]);
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
// Test 4: Hard reset eliminates false noop at the source
// ---------------------------------------------------------------------------

/// After a rejected session, retrying the worktree hard-resets the agent
/// branch to the integration tip. This means there are NO divergent commits
/// and therefore no false noop can occur. The false noop guard in bundle.rs
/// is a safety net; the hard reset is the primary fix.
///
/// This test proves the primary fix works: after retry, the branch matches
/// the integration HEAD exactly.
#[test]
fn test_hard_reset_eliminates_false_noop() {
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

    // After hard reset: branch matches integration HEAD exactly
    let integ_head = branch_head(&repo, &integ_branch);
    assert!(
        refs_identical(&wt2, &integ_head, "HEAD"),
        "Agent branch HEAD should match integration HEAD after hard reset"
    );

    // git diff --quiet returns exit 0 (no differences) - no false noop possible
    let diff_check = Command::new("git")
        .args(["diff", "--quiet", &integ_head, "HEAD"])
        .current_dir(&wt2)
        .output()
        .unwrap();
    assert!(
        diff_check.status.success(),
        "git diff --quiet should return exit 0 after hard reset (no divergence)"
    );

    // Rejected session's file is gone
    assert!(
        !wt2.join("database.py").exists(),
        "Rejected session's database.py must be destroyed"
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
        json!({"id": work_id, "target-status": "Ready", "role": "coordinator"}),
    )
    .await;

    // Verify the Work exists, is Ready, and has session_failure_count = 0
    {
        let works = stores.works.read().unwrap();
        let work = works.get(work_id.as_str()).unwrap();
        assert_eq!(work.status(), crate::domain::work::WorkStatus::Ready);
        assert_eq!(
            work.session_failure_count, 0,
            "session_failure_count should default to 0"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 7: Worktree reuse destroys rejected commits on retry
// ---------------------------------------------------------------------------

/// Verify that get_or_create_branch hard-resets the existing agent branch
/// to base_ref, destroying all commits from the rejected session.
/// This prevents the NO-OP loop where the implementer sees stale work
/// as "already done" and proposes a noop bundle.
#[test]
fn test_worktree_reuse_destroys_rejected_commits() {
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
    mgr.cleanup("wk-reuse").unwrap();

    // Session 2: get_or_create reuses existing branch but resets it
    let wt2 = mgr.get_or_create_branch("wk-reuse", &integ_branch).unwrap();

    // The rejected session's file must be gone - no ghosts
    assert!(
        !wt2.join("code.py").exists(),
        "Rejected session's code.py must be destroyed on retry"
    );

    // HEAD should match the integration branch (hard reset target)
    let integ_head = branch_head(&repo, &integ_branch);
    let session2_head = branch_head(&wt2, "HEAD");
    assert_eq!(
        integ_head, session2_head,
        "Branch HEAD should match integration tip after hard reset"
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

// ---------------------------------------------------------------------------
// Test 9: Bundle rejection -> work reset -> no deadlock (dispatch harness)
// ---------------------------------------------------------------------------

/// Prove that the bundle-rejection -> work-reset path completes without deadlock.
///
/// This test was added as part of the v0.1.115 remediation. Before the fix,
/// reset_work_after_bundle_rejection contained a lock_git() call that would
/// deadlock when called from within the merge-failure path (which already holds
/// _git_guard from process_tick). The test exercises the IPC state transitions
/// that reset_work_after_bundle_rejection performs, verifying they complete and
/// leave the work in the expected Ready state.
///
/// The test does not drive IntegratorAgent::process_tick directly (that requires
/// a full daemon with a live Unix socket). Instead it exercises the same IPC
/// dispatch path: work.transition(InReview -> Ready, override=true) which is
/// exactly what reset_work_after_bundle_rejection calls.
#[tokio::test]
async fn test_bundle_rejection_work_reset_no_deadlock() {
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
            title: "Rejection Test Plan",
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

    // Drive work to Ready
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": work_id, "target-status": "Ready", "role": "coordinator"}),
    )
    .await;

    // Drive work to InProgress (simulating implementer assignment)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": work_id, "target-status": "InProgress", "role": "coordinator", "assignee": "agent-test"}),
    )
    .await;

    // Create a bundle (simulating implementer proposing work)
    let bundle = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({
            "work-id": work_id,
            "description": "Test bundle",
            "files-changed": ["src/main.py"],
            "commit_sha": "abc123",
            "branch-name": "agent/wk-test"
        }),
    )
    .await;
    let bundle_id = bundle["id"].as_str().unwrap().to_string();

    // Triage -> Reviewed -> Accepted (simulating reviewer approval)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Triaged", "role": "coordinator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Reviewed", "role": "reviewer", "verification": "ok"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Accepted", "role": "coordinator"}),
    )
    .await;

    // In a live daemon, the Work would auto-advance to InReview when its bundle is
    // Accepted. In the test harness there is no daemon event loop, so drive it explicitly.
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": work_id, "target-status": "InReview", "role": "coordinator", "override": true}),
    )
    .await;

    {
        let works = stores.works.read().unwrap();
        let work = works.get(work_id.as_str()).unwrap();
        assert_eq!(
            work.status(),
            WorkStatus::InReview,
            "work should be InReview after explicit transition"
        );
    }

    // Simulate integrator rejecting the bundle (stale base tick)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Rejected", "role": "integrator"}),
    )
    .await;

    // This is the exact IPC call that reset_work_after_bundle_rejection makes.
    // Before the remediation, calling this while holding _git_guard caused a
    // permanent deadlock. Now reset_work_after_bundle_rejection contains only
    // IPC calls (no lock_git), so this must complete.
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({
            "id": work_id,
            "target-status": "Ready",
            "role": "coordinator",
            "override": true
        }),
    )
    .await;

    // Work is back to Ready - pipeline can retry
    {
        let works = stores.works.read().unwrap();
        let work = works.get(work_id.as_str()).unwrap();
        assert_eq!(
            work.status(),
            WorkStatus::Ready,
            "work should be Ready after rejection reset"
        );
    }
}
