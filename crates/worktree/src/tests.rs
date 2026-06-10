use std::path::{Path, PathBuf};

use super::*;

// ---------- delete_branch / cleanup_at guards (Phase-5 finding 12) ----------

#[test]
fn delete_branch_rejects_non_loopr_branch() {
    // A buggy caller must never reach `git branch -D main`.
    let err = delete_branch(Path::new("/tmp/repo"), "main").unwrap_err();
    assert!(matches!(err, WorktreeError::InvalidBranchName(b) if b == "main"));
}

#[test]
fn delete_branch_rejects_plain_feature_branch() {
    let err = delete_branch(Path::new("/tmp/repo"), "feature/x").unwrap_err();
    assert!(matches!(err, WorktreeError::InvalidBranchName(_)));
}

#[test]
fn under_worktrees_root_accepts_loopr_worktree_path() {
    assert!(under_worktrees_root(Path::new(
        "/home/me/proj/.loopr/worktrees/wk-abc12-1"
    )));
}

#[test]
fn under_worktrees_root_rejects_arbitrary_path() {
    assert!(!under_worktrees_root(Path::new("/home/me/proj/src")));
    assert!(!under_worktrees_root(Path::new("/home/me")));
    // `.loopr` without the `worktrees` child does not qualify.
    assert!(!under_worktrees_root(Path::new("/home/me/proj/.loopr/records/x")));
}

#[test]
fn cleanup_at_rejects_path_outside_worktrees_root() {
    let err = cleanup_at(Path::new("/tmp/repo"), &PathBuf::from("/home/me/proj/src")).unwrap_err();
    assert!(matches!(err, WorktreeError::NotFound(_)));
}
