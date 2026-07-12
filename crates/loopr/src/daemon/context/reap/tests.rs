//! Tests for `reap_terminal_work_worktree`.
//!
//! Uses a real `git init`'d repo and real `worktree::Worktree` handles
//! (same idiom as `daemon::startup::tests`), so the `worktree::list` +
//! `parse_branch` + `cleanup_at` + `delete_branch` round-trip runs
//! end-to-end against real git, not a fake.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use domain::WorkStatus;
use worktree::Worktree;

use super::reap_terminal_work_worktree;

fn seed_repo(path: &Path) {
    for args in [
        &["init", "-q", "--initial-branch=main"][..],
        &["config", "user.email", "test@example.com"][..],
        &["config", "user.name", "Test"][..],
        &["config", "commit.gpgsign", "false"][..],
        &["config", "tag.gpgsign", "false"][..],
        &["commit", "-q", "--allow-empty", "-m", "init"][..],
    ] {
        let out = std::process::Command::new("git")
            .current_dir(path)
            .env("LC_ALL", "C")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn resolve_head(path: &Path) -> String {
    let out = std::process::Command::new("git")
        .current_dir(path)
        .env("LC_ALL", "C")
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .env("LC_ALL", "C")
        .args(["branch", "--list", branch])
        .output()
        .unwrap();
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

/// Create a worktree for a fresh `WorkId` and leak the handle (so its
/// on-disk directory + branch survive for the test to act on), returning
/// `(work_id, path, branch)`.
fn create_leaked_worktree(repo: &Path) -> (domain::WorkId, std::path::PathBuf, String) {
    let sha = resolve_head(repo);
    let wt_root = repo.join(".loopr").join("worktrees");
    let wt = Worktree::create(repo, &wt_root, domain::WorkId::new(), &sha).unwrap();
    let work_id = wt.work_id().clone();
    let path = wt.path().to_path_buf();
    let branch = wt.branch().to_string();
    std::mem::forget(wt);
    (work_id, path, branch)
}

/// Break-to-prove: a `Work` still in flight (here, `InReview` -- the exact
/// state Phase 10 retains warm across review) must NEVER have its worktree
/// or branch removed. This is the literal "a Work in InReview retains its
/// worktree" success criterion from Phase 19.
#[tokio::test]
async fn reap_refuses_non_terminal_status_in_review() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    let (work_id, path, branch) = create_leaked_worktree(&repo);

    reap_terminal_work_worktree(&repo, &work_id, WorkStatus::InReview).await;

    assert!(path.exists(), "InReview worktree must survive reap");
    assert!(branch_exists(&repo, &branch), "InReview branch must survive reap");
}

/// Same guard, `Blocked` (the state an IntegrationFailed Bundle drives its
/// Work into while retryable) -- also non-terminal, also retained.
#[tokio::test]
async fn reap_refuses_non_terminal_status_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    let (work_id, path, branch) = create_leaked_worktree(&repo);

    reap_terminal_work_worktree(&repo, &work_id, WorkStatus::Blocked).await;

    assert!(path.exists(), "Blocked (retryable) worktree must survive reap");
    assert!(
        branch_exists(&repo, &branch),
        "Blocked (retryable) branch must survive reap"
    );
}

/// Break-to-prove: `Done` (the normal success terminal state) reaps both
/// the worktree directory and the branch.
#[tokio::test]
async fn reap_removes_worktree_and_branch_on_done() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    let (work_id, path, branch) = create_leaked_worktree(&repo);

    reap_terminal_work_worktree(&repo, &work_id, WorkStatus::Done).await;

    assert!(!path.exists(), "Done worktree dir should be reaped");
    assert!(!branch_exists(&repo, &branch), "Done branch should be reaped");
}

/// Phase 19's actual extension: `Abandoned` (an IntegrationFailed Bundle
/// whose Work the Director eventually gave up on) reaps both the worktree
/// AND the branch, not only `Done`.
#[tokio::test]
async fn reap_removes_worktree_and_branch_on_abandoned() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    let (work_id, path, branch) = create_leaked_worktree(&repo);

    reap_terminal_work_worktree(&repo, &work_id, WorkStatus::Abandoned).await;

    assert!(!path.exists(), "Abandoned worktree dir should be reaped");
    assert!(!branch_exists(&repo, &branch), "Abandoned branch should be reaped");
}

/// `Superseded` gets the same terminal treatment as `Done`/`Abandoned`.
#[tokio::test]
async fn reap_removes_worktree_and_branch_on_superseded() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    let (work_id, path, branch) = create_leaked_worktree(&repo);

    reap_terminal_work_worktree(&repo, &work_id, WorkStatus::Superseded).await;

    assert!(!path.exists(), "Superseded worktree dir should be reaped");
    assert!(!branch_exists(&repo, &branch), "Superseded branch should be reaped");
}

/// A terminal status with no matching worktree on disk is a benign no-op
/// (nothing to reap, nothing to error on).
#[tokio::test]
async fn reap_is_noop_when_no_worktree_matches_work_id() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    // No worktree_root even exists yet.
    reap_terminal_work_worktree(&repo, &domain::WorkId::new(), WorkStatus::Done).await;

    // A worktree_root exists but has no entry for this work_id.
    let (_other_work_id, other_path, _other_branch) = create_leaked_worktree(&repo);
    reap_terminal_work_worktree(&repo, &domain::WorkId::new(), WorkStatus::Done).await;
    assert!(other_path.exists(), "unrelated Work's worktree must be untouched");
}

/// A Work retried after a failed attempt accumulates worktrees at
/// different `seq` numbers under the same `work_id` (no branch reuse
/// across attempts, per the worktree crate's contract). Reaping on the
/// eventual terminal status must sweep EVERY seq for that `work_id`, not
/// just the most recent one.
#[tokio::test]
async fn reap_sweeps_every_seq_for_the_same_work_id() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    let sha = resolve_head(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");
    let work_id = domain::WorkId::new();

    // Attempt 1 (seq 1): failed, orphaned worktree left on disk.
    let wt1 = Worktree::create(&repo, &wt_root, work_id.clone(), &sha).unwrap();
    let path1 = wt1.path().to_path_buf();
    let branch1 = wt1.branch().to_string();
    std::mem::forget(wt1);

    // Attempt 2 (seq 2): the retry's worktree, also left warm.
    let wt2 = Worktree::create(&repo, &wt_root, work_id.clone(), &sha).unwrap();
    let path2 = wt2.path().to_path_buf();
    let branch2 = wt2.branch().to_string();
    std::mem::forget(wt2);

    assert_ne!(path1, path2, "each attempt gets its own seq-numbered path");

    reap_terminal_work_worktree(&repo, &work_id, WorkStatus::Done).await;

    assert!(!path1.exists(), "seq 1 worktree should be reaped");
    assert!(!path2.exists(), "seq 2 worktree should be reaped");
    assert!(!branch_exists(&repo, &branch1), "seq 1 branch should be reaped");
    assert!(!branch_exists(&repo, &branch2), "seq 2 branch should be reaped");
}
