//! Handle tests. Phase 3: real-git integration tests that exercise
//! `Worktree::create`, the seq-retry loop, and Drop cleanup, plus
//! unit-style assertions on the accessors and `consumed` flag.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::str::FromStr;

use domain::WorkId;

use crate::ops;

use super::*;

fn wk(prefix: &str) -> WorkId {
    WorkId::from_str(prefix).unwrap()
}

/// Initialize a fresh git repo at `path` with a single seed commit and
/// return the commit SHA. Disables GPG signing so tests are hermetic
/// regardless of the host's `~/.gitconfig`.
fn seed_repo(path: &Path) -> String {
    for args in [
        &["init", "-q", "--initial-branch=main"][..],
        &["config", "user.email", "test@example.com"][..],
        &["config", "user.name", "Test"][..],
        &["config", "commit.gpgsign", "false"][..],
        &["config", "tag.gpgsign", "false"][..],
        &["commit", "-q", "--allow-empty", "-m", "init"][..],
    ] {
        let out = ops::git_cmd(path).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = ops::git_cmd(path).args(["rev-parse", "HEAD"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    let out = ops::git_cmd(repo).args(["branch", "--list", branch]).output().unwrap();
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

#[test]
fn accessors_return_stored_fields() {
    let wt = Worktree::from_parts(
        PathBuf::from("/tmp/target/.loopr/worktrees/wk-abc12-1"),
        "loopr/wk-wk-abc12-1".to_string(),
        wk("wk-abc12"),
        1,
        PathBuf::from("/tmp/target"),
        true,
    );
    assert_eq!(
        wt.path(),
        std::path::Path::new("/tmp/target/.loopr/worktrees/wk-abc12-1")
    );
    assert_eq!(wt.branch(), "loopr/wk-wk-abc12-1");
    assert_eq!(wt.work_id().as_ref(), "wk-abc12");
    assert_eq!(wt.seq(), 1);
}

#[test]
fn drop_with_consumed_is_a_noop() {
    let wt = Worktree::from_parts(
        PathBuf::from("/definitely/does/not/exist"),
        "loopr/wk-wk-abc12-1".to_string(),
        wk("wk-abc12"),
        1,
        PathBuf::from("/also/nonexistent"),
        true,
    );
    drop(wt);
}

#[test]
fn create_succeeds_at_seq_1_on_fresh_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);

    let wt_root = repo.join(".loopr").join("worktrees");
    let wt = Worktree::create(&repo, &wt_root, wk("wk-abc12"), &sha).unwrap();

    assert_eq!(wt.seq(), 1);
    assert_eq!(wt.branch(), "loopr/wk-wk-abc12-1");
    assert_eq!(wt.path(), wt_root.join("wk-abc12-1"));
    assert!(wt.path().exists());
    // Cleanup so Drop doesn't warn on a tempdir that will be gone.
    wt.cleanup().unwrap();
}

#[test]
fn create_allocates_next_seq_when_prior_branch_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");

    // Create first attempt; cleanup keeps the branch alive.
    let wt1 = Worktree::create(&repo, &wt_root, wk("wk-abc12"), &sha).unwrap();
    assert_eq!(wt1.seq(), 1);
    wt1.cleanup().unwrap();
    assert!(branch_exists(&repo, "loopr/wk-wk-abc12-1"));

    // Second attempt: branch-name collision on seq=1 forces seq=2.
    let wt2 = Worktree::create(&repo, &wt_root, wk("wk-abc12"), &sha).unwrap();
    assert_eq!(wt2.seq(), 2, "branch collision on seq=1 must bump to seq=2");
    assert_eq!(wt2.branch(), "loopr/wk-wk-abc12-2");
    wt2.cleanup().unwrap();
}

#[test]
fn two_sequential_creates_without_cleanup_both_coexist() {
    // Parallel/queued attempt for the same work_id while the first is
    // still live must allocate to seq=2.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");

    let wt1 = Worktree::create(&repo, &wt_root, wk("wk-abc12"), &sha).unwrap();
    let wt2 = Worktree::create(&repo, &wt_root, wk("wk-abc12"), &sha).unwrap();

    assert_eq!(wt1.seq(), 1);
    assert_eq!(wt2.seq(), 2);
    assert!(wt1.path().exists());
    assert!(wt2.path().exists());

    wt2.cleanup().unwrap();
    wt1.cleanup().unwrap();
}

#[test]
fn drop_cleans_worktree_keeps_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");

    let path;
    let branch;
    {
        let wt = Worktree::create(&repo, &wt_root, wk("wk-abc12"), &sha).unwrap();
        path = wt.path().to_path_buf();
        branch = wt.branch().to_string();
        // wt drops here
    }
    assert!(!path.exists(), "Drop must remove the worktree dir");
    assert!(branch_exists(&repo, &branch), "Drop must NOT delete the branch");
}

#[test]
fn cleanup_removes_worktree_keeps_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");

    let wt = Worktree::create(&repo, &wt_root, wk("wk-abc12"), &sha).unwrap();
    let path = wt.path().to_path_buf();
    let branch = wt.branch().to_string();

    wt.cleanup().unwrap();

    assert!(!path.exists());
    assert!(branch_exists(&repo, &branch));
}

#[test]
fn create_sha_becomes_worktree_head() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");

    let wt = Worktree::create(&repo, &wt_root, wk("wk-abc12"), &sha).unwrap();

    let out = ops::git_cmd(wt.path()).args(["rev-parse", "HEAD"]).output().unwrap();
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(head, sha, "worktree HEAD must match the sha we passed");

    wt.cleanup().unwrap();
}

#[test]
fn concurrent_creates_for_same_work_id_all_get_distinct_seqs() {
    // 10 threads race to create worktrees for the same work_id.
    // Git's internal serialization of `worktree add` + the SeqTaken retry
    // loop must produce 10 distinct seq values without any error.
    use std::sync::Arc;
    use std::thread;

    let tmp = Arc::new(tempfile::tempdir().unwrap());
    let repo = Arc::new(tmp.path().join("repo"));
    std::fs::create_dir(repo.as_ref()).unwrap();
    let sha = Arc::new(seed_repo(&repo));
    let wt_root = Arc::new(repo.join(".loopr").join("worktrees"));

    const THREADS: u32 = 10;
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let repo = Arc::clone(&repo);
            let wt_root = Arc::clone(&wt_root);
            let sha = Arc::clone(&sha);
            thread::spawn(move || Worktree::create(&repo, &wt_root, wk("wk-seqtest"), &sha).unwrap())
        })
        .collect();

    let mut seqs: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap().seq()).collect();
    seqs.sort_unstable();
    // All 10 succeed; seqs are contiguous 1..=10 (no gaps, no duplicates).
    assert_eq!(seqs, (1..=THREADS).collect::<Vec<_>>());
}
