use std::fs;

use tempfile::TempDir;

use super::{MAX_ENTRIES, collect_workspace_tree};

/// Create a git repo at `target` with `git init` + an initial empty
/// commit. Returns `Ok(())` only when git is on PATH and the commands
/// succeed; tests that need git bail early on `Err`.
fn git_init(target: &std::path::Path) -> std::io::Result<()> {
    let run = |args: &[&str]| -> std::io::Result<()> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(target)
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    };
    run(&["init", "--quiet"])?;
    run(&["config", "user.email", "test@example.com"])?;
    run(&["config", "user.name", "test"])?;
    Ok(())
}

#[test]
fn missing_target_errors_as_workspace_scan_failed() {
    let dir = TempDir::new().expect("tempdir");
    let missing = dir.path().join("does-not-exist");
    let err = collect_workspace_tree(&missing).expect_err("should fail");
    match err {
        crate::DecomposerError::WorkspaceScanFailed(msg) => {
            assert!(msg.contains("does not exist"), "got: {msg}");
        }
        other => panic!("expected WorkspaceScanFailed, got {other:?}"),
    }
}

#[test]
fn file_target_errors_as_workspace_scan_failed() {
    let dir = TempDir::new().expect("tempdir");
    let file = dir.path().join("somefile");
    fs::write(&file, "x").expect("write");
    let err = collect_workspace_tree(&file).expect_err("should fail");
    match err {
        crate::DecomposerError::WorkspaceScanFailed(msg) => {
            assert!(msg.contains("not a directory"), "got: {msg}");
        }
        other => panic!("expected WorkspaceScanFailed, got {other:?}"),
    }
}

#[test]
fn empty_non_git_target_returns_empty_sentinel() {
    let dir = TempDir::new().expect("tempdir");
    let tree = collect_workspace_tree(dir.path()).expect("collect");
    assert_eq!(tree, "(empty workspace)");
}

#[test]
fn non_git_target_with_files_walks_with_skip_list() {
    let dir = TempDir::new().expect("tempdir");
    fs::write(dir.path().join("README.md"), "readme").expect("write");
    fs::create_dir(dir.path().join("src")).expect("mkdir src");
    fs::write(dir.path().join("src").join("main.rs"), "fn main() {}").expect("write main");

    // These should be skipped by the fallback walk's SKIP_DIRS.
    fs::create_dir(dir.path().join("target")).expect("mkdir target");
    fs::write(dir.path().join("target").join("binary"), b"ignored").expect("write");
    fs::create_dir(dir.path().join("node_modules")).expect("mkdir node_modules");
    fs::write(dir.path().join("node_modules").join("dep.js"), b"should not appear").expect("write");
    // Dot-prefixed directory skipped too.
    fs::create_dir(dir.path().join(".cache")).expect("mkdir .cache");
    fs::write(dir.path().join(".cache").join("blob"), b"").expect("write");

    let tree = collect_workspace_tree(dir.path()).expect("collect");
    assert!(tree.contains("README.md"), "tree: {tree}");
    assert!(tree.contains("src/main.rs"), "tree: {tree}");
    assert!(!tree.contains("target/binary"), "target/ should be skipped: {tree}");
    assert!(
        !tree.contains("node_modules"),
        "node_modules/ should be skipped: {tree}"
    );
    assert!(!tree.contains(".cache"), "dot-prefixed dirs should be skipped: {tree}");
}

#[test]
fn git_repo_with_tracked_and_untracked_not_ignored_files() {
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path();
    if git_init(target).is_err() {
        eprintln!("skipping: git not available on PATH");
        return;
    }

    fs::write(target.join("tracked.md"), "tracked").expect("write");
    let add = std::process::Command::new("git")
        .args(["add", "tracked.md"])
        .current_dir(target)
        .output()
        .expect("git add");
    assert!(add.status.success());
    let commit = std::process::Command::new("git")
        .args(["commit", "--quiet", "-m", "init"])
        .current_dir(target)
        .output()
        .expect("git commit");
    assert!(commit.status.success(), "commit failed: {commit:?}");

    // Untracked-not-ignored file.
    fs::write(target.join("untracked.md"), "untracked").expect("write untracked");
    // Ignored file (via .gitignore).
    fs::write(target.join(".gitignore"), "ignored.log\n").expect("write gitignore");
    fs::write(target.join("ignored.log"), "nope").expect("write ignored");

    let tree = collect_workspace_tree(target).expect("collect");
    assert!(tree.contains("tracked.md"), "tree: {tree}");
    assert!(tree.contains("untracked.md"), "tree: {tree}");
    assert!(!tree.contains("ignored.log"), "tree should exclude ignored.log: {tree}");
}

#[test]
fn entry_cap_truncates_with_marker() {
    let dir = TempDir::new().expect("tempdir");
    // Create MAX_ENTRIES + 10 files in the non-git target so the
    // fallback walker hits the cap.
    let count = MAX_ENTRIES + 10;
    for i in 0..count {
        fs::write(dir.path().join(format!("file-{i:04}.txt")), "x").expect("write");
    }

    let tree = collect_workspace_tree(dir.path()).expect("collect");
    let line_count = tree.lines().count();
    // MAX_ENTRIES visible lines + 1 truncation marker line.
    assert_eq!(line_count, MAX_ENTRIES + 1, "tree line count: {line_count}");
    assert!(
        tree.contains(&format!("... and {} more entries", count - MAX_ENTRIES)),
        "truncation marker missing: {tree}"
    );
}
