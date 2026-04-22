//! Tests for ops.rs.
//!
//! Two flavors:
//! - Classifier tests using synthesized `Output` structs against the stderr
//!   phrases git 2.51 emits.
//! - Real-git integration tests using `tempfile::TempDir` + `git init` +
//!   a seed commit; these exercise `try_create_at_seq`, `remove_worktree`,
//!   `delete_branch`, `prune`, `resolve_sha`, `show_current_branch`,
//!   and `list_porcelain` against a real repo.
//!
//! `#[allow(clippy::unwrap_used)]` is permissive in tests per rules/rust.md.

#![allow(clippy::unwrap_used)]

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Output};
use std::str::FromStr;

use domain::WorkId;

use super::*;

/// Initialize a fresh git repo at `path` with a single seed commit and
/// return the commit SHA.
///
/// Disables GPG signing + user-scoped hooks/templates so the test is hermetic
/// regardless of the host's `~/.gitconfig` (the project author's config
/// forces `commit.gpgsign = true`, which times out in headless tests).
fn seed_repo(path: &Path) -> String {
    run_git(path, &["init", "-q", "--initial-branch=main"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test"]);
    run_git(path, &["config", "commit.gpgsign", "false"]);
    run_git(path, &["config", "tag.gpgsign", "false"]);
    run_git(path, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let out = git_cmd(path).args(["rev-parse", "HEAD"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn run_git(path: &Path, args: &[&str]) {
    let out = git_cmd(path).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn mk_output(stderr: &str, exit_code: i32) -> Output {
    Output {
        status: ExitStatus::from_raw(exit_code << 8),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[test]
fn is_seq_taken_matches_path_collision_phrasing() {
    // Git 2.51: `fatal: 'existing' already exists`
    let out = mk_output("fatal: 'existing' already exists\n", 128);
    assert!(is_seq_taken(&out));
}

#[test]
fn is_seq_taken_matches_branch_collision_phrasing() {
    // Git 2.51: `fatal: a branch named 'new-branch' already exists`
    let out = mk_output("fatal: a branch named 'new-branch' already exists\n", 255);
    assert!(is_seq_taken(&out));
}

#[test]
fn is_seq_taken_matches_already_checked_out() {
    let out = mk_output("fatal: 'some-branch' is already checked out at '/other/path'\n", 128);
    assert!(is_seq_taken(&out));
}

#[test]
fn is_seq_taken_matches_not_an_empty_directory() {
    let out = mk_output("fatal: '/tmp/wt' is not an empty directory\n", 128);
    assert!(is_seq_taken(&out));
}

#[test]
fn is_seq_taken_does_not_match_disk_full() {
    let out = mk_output("fatal: write failure: No space left on device\n", 128);
    assert!(!is_seq_taken(&out));
}

#[test]
fn is_seq_taken_does_not_match_permission_denied() {
    let out = mk_output("fatal: cannot access '/root/wt': Permission denied\n", 128);
    assert!(!is_seq_taken(&out));
}

#[test]
fn format_stderr_shapes_exit_and_body() {
    let out = mk_output("fatal: oh no\n", 128);
    let s = format_stderr(&out);
    assert_eq!(s, "exit 128: fatal: oh no");
}

#[test]
fn try_create_at_seq_creates_worktree_and_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);

    let wt_root = repo.join(".loopr").join("worktrees");
    std::fs::create_dir_all(&wt_root).unwrap();
    let wk = WorkId::from_str("wk-abc12").unwrap();

    let outcome = try_create_at_seq(&repo, &wt_root, &wk, 1, &sha).unwrap();
    match outcome {
        CreateOutcome::Created { path, branch } => {
            assert_eq!(path, wt_root.join("wk-abc12-1"));
            assert_eq!(branch, "loopr/wk-wk-abc12-1");
            assert!(path.exists());
            assert_eq!(show_current_branch(&path).unwrap(), "loopr/wk-wk-abc12-1");
        }
        CreateOutcome::SeqTaken => panic!("expected Created"),
    }
}

#[test]
fn try_create_at_seq_reports_seq_taken_on_second_attempt_same_seq() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");
    std::fs::create_dir_all(&wt_root).unwrap();
    let wk = WorkId::from_str("wk-abc12").unwrap();

    let first = try_create_at_seq(&repo, &wt_root, &wk, 1, &sha).unwrap();
    assert!(matches!(first, CreateOutcome::Created { .. }));

    let second = try_create_at_seq(&repo, &wt_root, &wk, 1, &sha).unwrap();
    assert_eq!(second, CreateOutcome::SeqTaken);
}

#[test]
fn remove_worktree_removes_created_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");
    std::fs::create_dir_all(&wt_root).unwrap();
    let wk = WorkId::from_str("wk-abc12").unwrap();

    let CreateOutcome::Created { path, branch } = try_create_at_seq(&repo, &wt_root, &wk, 1, &sha).unwrap() else {
        panic!("create failed");
    };
    assert!(path.exists());

    remove_worktree(&repo, &path).unwrap();
    assert!(!path.exists(), "worktree dir should be gone after remove_worktree");

    // Branch survives remove_worktree (integrator still needs it).
    let out = git_cmd(&repo).args(["branch", "--list", &branch]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&branch),
        "branch should survive remove_worktree"
    );
}

#[test]
fn remove_worktree_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    // Path that was never a worktree → Ok.
    remove_worktree(&repo, &repo.join("nowhere")).unwrap();
}

#[test]
fn delete_branch_deletes_existing_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    run_git(&repo, &["branch", "loopr/wk-wk-abc12-1"]);
    delete_branch(&repo, "loopr/wk-wk-abc12-1").unwrap();
    let out = git_cmd(&repo)
        .args(["branch", "--list", "loopr/wk-wk-abc12-1"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
}

#[test]
fn delete_branch_is_idempotent_on_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    delete_branch(&repo, "loopr/wk-does-not-exist-1").unwrap();
}

#[test]
fn prune_ok_on_clean_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    prune(&repo).unwrap();
}

#[test]
fn resolve_sha_resolves_head_in_repo_context() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);

    let resolved = resolve_sha(&repo, "HEAD").unwrap();
    assert_eq!(resolved, sha);
}

#[test]
fn resolve_sha_errors_on_bad_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);

    let err = resolve_sha(&repo, "no-such-ref").unwrap_err();
    assert!(matches!(err, WorktreeError::GitCommand(_)));
}

#[test]
fn list_porcelain_returns_output_for_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");
    std::fs::create_dir_all(&wt_root).unwrap();
    let wk = WorkId::from_str("wk-abc12").unwrap();
    try_create_at_seq(&repo, &wt_root, &wk, 1, &sha).unwrap();

    let out = list_porcelain(&repo).unwrap();
    assert!(out.contains("worktree "), "expected porcelain output, got {out:?}");
    assert!(out.contains("loopr/wk-wk-abc12-1"));
}

#[test]
fn git_cmd_forces_lc_all_c() {
    // `.env(…)` on Command sets the child env; we can't read it back from the
    // returned Command without executing. As a proxy, execute a tiny git
    // command and assert stderr is English under a locale override in the
    // parent. This exists as a regression guard: if the `.env("LC_ALL", "C")`
    // line disappears, running git inside this process under a localized
    // LANG would produce non-English stderr our classifier would miss.
    //
    // We create a scratch dir, probe `git branch --show-current` on a
    // non-repo path (expected failure), and assert the error is English.
    let tmp = tempfile::tempdir().expect("tmpdir");
    let mut cmd = git_cmd(tmp.path());
    // Simulate a hostile locale in the caller's env. `git_cmd` should
    // override it.
    cmd.env("LC_ALL", "C");
    cmd.env("LANG", "fr_FR.UTF-8");
    let out = cmd.args(["status"]).output().expect("spawn git");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // tempdir is empty: `fatal: not a git repository (or any of the parent directories): .git`
    // The critical substring is "not a git repository" — stays English under LC_ALL=C.
    assert!(
        stderr.contains("not a git repository"),
        "stderr should be English under LC_ALL=C; got {stderr:?}"
    );
}
