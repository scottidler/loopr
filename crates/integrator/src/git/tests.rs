//! Tests for the Phase C no-branch-override git helpers: `current_branch`
//! and `working_tree_dirty`. These back the cleanliness guard that keeps
//! the override from merging onto an operator's uncommitted work.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;

use super::{current_branch, working_tree_dirty};

const TIMEOUT: Duration = Duration::from_secs(30);

/// `git init -b main` with a single empty commit so HEAD resolves.
fn init_repo(path: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(path).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    run(&["config", "commit.gpgsign", "false"]);
    run(&["commit", "--allow-empty", "-q", "-m", "init"]);
}

#[tokio::test]
async fn current_branch_returns_checked_out_branch() {
    let td = TempDir::new().unwrap();
    init_repo(td.path());
    assert_eq!(current_branch(td.path(), TIMEOUT).await.unwrap(), "main");
}

#[tokio::test]
async fn working_tree_dirty_is_false_when_clean() {
    let td = TempDir::new().unwrap();
    init_repo(td.path());
    assert!(
        !working_tree_dirty(td.path(), TIMEOUT).await.unwrap(),
        "clean tree must not be dirty"
    );
}

#[tokio::test]
async fn working_tree_dirty_is_true_with_untracked_file() {
    let td = TempDir::new().unwrap();
    init_repo(td.path());
    std::fs::write(td.path().join("scratch.txt"), b"uncommitted\n").unwrap();
    assert!(
        working_tree_dirty(td.path(), TIMEOUT).await.unwrap(),
        "an untracked file must register as dirty (conservative override guard)"
    );
}

#[tokio::test]
async fn working_tree_dirty_is_true_with_tracked_modification() {
    let td = TempDir::new().unwrap();
    init_repo(td.path());
    let f = td.path().join("tracked.txt");
    std::fs::write(&f, b"v1\n").unwrap();
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(td.path())
            .args(args)
            .output()
            .unwrap();
    };
    run(&["add", "tracked.txt"]);
    run(&["commit", "-q", "-m", "add tracked"]);
    std::fs::write(&f, b"v2\n").unwrap();
    assert!(
        working_tree_dirty(td.path(), TIMEOUT).await.unwrap(),
        "a modified tracked file must register as dirty"
    );
}

/// loopr's own run-local state under `.loopr/` (here an UNTRACKED
/// `.loopr/taskstore/`, the test-harness / pre-first-commit shape) must NOT
/// count as dirty — it is loopr's bookkeeping, not the operator's work.
/// Break-to-prove: against the pre-fix plain `git status --porcelain` this
/// returns `?? .loopr/` and the assertion below fails (dirty == true), which
/// is exactly what wedged the integrator in the reject->retry->accept path.
#[tokio::test]
async fn working_tree_dirty_ignores_untracked_loopr_state() {
    let td = TempDir::new().unwrap();
    init_repo(td.path());
    std::fs::create_dir_all(td.path().join(".loopr").join("taskstore")).unwrap();
    std::fs::write(td.path().join(".loopr").join("taskstore").join("plans.jsonl"), b"{}\n").unwrap();
    assert!(
        !working_tree_dirty(td.path(), TIMEOUT).await.unwrap(),
        "loopr's own untracked .loopr/ state must not register as dirty"
    );
}

/// The production shape: `.loopr/taskstore/` is committed (per vision) but
/// goes dirty between the taskstore git-hook commits. A modified TRACKED
/// taskstore file is still loopr's own state and must not trip the guard.
#[tokio::test]
async fn working_tree_dirty_ignores_modified_tracked_loopr_taskstore() {
    let td = TempDir::new().unwrap();
    init_repo(td.path());
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(td.path())
            .args(args)
            .output()
            .unwrap();
    };
    std::fs::create_dir_all(td.path().join(".loopr").join("taskstore")).unwrap();
    let jsonl = td.path().join(".loopr").join("taskstore").join("plans.jsonl");
    std::fs::write(&jsonl, b"{}\n").unwrap();
    run(&["add", "-f", ".loopr/taskstore/plans.jsonl"]);
    run(&["commit", "-q", "-m", "commit taskstore"]);
    std::fs::write(&jsonl, b"{\"updated\":1}\n").unwrap();
    assert!(
        !working_tree_dirty(td.path(), TIMEOUT).await.unwrap(),
        "a modified tracked .loopr/taskstore file is loopr's own state, not operator work"
    );
}

/// The guard must still bite: genuine operator work OUTSIDE `.loopr/` is
/// reported dirty even when loopr's own `.loopr/` state is also present, so
/// the exclusion narrows the guard without blinding it.
#[tokio::test]
async fn working_tree_dirty_still_reports_operator_work_alongside_loopr() {
    let td = TempDir::new().unwrap();
    init_repo(td.path());
    std::fs::create_dir_all(td.path().join(".loopr").join("taskstore")).unwrap();
    std::fs::write(td.path().join(".loopr").join("taskstore").join("plans.jsonl"), b"{}\n").unwrap();
    std::fs::write(td.path().join("operator.txt"), b"uncommitted operator edit\n").unwrap();
    assert!(
        working_tree_dirty(td.path(), TIMEOUT).await.unwrap(),
        "operator work outside .loopr/ must still register as dirty"
    );
}
