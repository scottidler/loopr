use super::*;

#[test]
fn display_git_command() {
    let e = WorktreeError::GitCommand("stderr blurb".into());
    assert_eq!(e.to_string(), "git command failed: stderr blurb");
}

#[test]
fn display_not_found() {
    let e = WorktreeError::NotFound(PathBuf::from("/tmp/nope"));
    assert_eq!(e.to_string(), "worktree not found at /tmp/nope");
}

#[test]
fn display_seq_alloc_exhausted() {
    let e = WorktreeError::SeqAllocExhausted {
        attempts: 1000,
        dir: PathBuf::from("/tmp/wts"),
    };
    assert_eq!(
        e.to_string(),
        "failed to allocate seq after 1000 attempts under /tmp/wts"
    );
}

#[test]
fn display_invalid_branch_name() {
    let e = WorktreeError::InvalidBranchName("main".into());
    assert_eq!(
        e.to_string(),
        "invalid branch name \"main\": not a loopr-managed branch"
    );
}

#[test]
fn io_conversion() {
    let io = std::io::Error::other("boom");
    let e: WorktreeError = io.into();
    assert!(matches!(e, WorktreeError::Io(_)));
}
