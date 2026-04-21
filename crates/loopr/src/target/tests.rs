#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as PCommand;

use tempfile::TempDir;

use super::*;

fn git_init(dir: &Path) {
    let status = PCommand::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git init failed");
}

#[test]
fn chdir_takes_precedence_over_env_and_cwd() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let c = TempDir::new().unwrap();
    let resolved = resolve(Some(a.path()), Some(b.path().to_str().unwrap()), c.path()).unwrap();
    assert_eq!(resolved, a.path().canonicalize().unwrap());
}

#[test]
fn env_takes_precedence_over_cwd() {
    let b = TempDir::new().unwrap();
    let c = TempDir::new().unwrap();
    let resolved = resolve(None, Some(b.path().to_str().unwrap()), c.path()).unwrap();
    assert_eq!(resolved, b.path().canonicalize().unwrap());
}

#[test]
fn cwd_used_when_chdir_and_env_unset() {
    let c = TempDir::new().unwrap();
    let resolved = resolve(None, None, c.path()).unwrap();
    assert_eq!(resolved, c.path().canonicalize().unwrap());
}

#[test]
fn empty_env_string_treated_as_unset() {
    let c = TempDir::new().unwrap();
    let resolved = resolve(None, Some(""), c.path()).unwrap();
    assert_eq!(resolved, c.path().canonicalize().unwrap());
}

#[test]
fn invalid_path_errors() {
    let fake = PathBuf::from("/does/not/exist/anywhere/42");
    let err = resolve(Some(&fake), None, Path::new("/tmp")).unwrap_err();
    assert!(matches!(err, LooprError::TargetInvalid { .. }));
}

#[test]
fn file_path_errors_as_target_is_file_with_parent_hint() {
    let td = TempDir::new().unwrap();
    let file = td.path().join("a-file");
    fs::write(&file, "").unwrap();
    let err = resolve(Some(&file), None, Path::new("/tmp")).unwrap_err();
    match err {
        LooprError::TargetIsFile { ref path } => {
            assert_eq!(path, &file);
            let msg = err.to_string();
            assert!(msg.contains("is a file"), "got: {msg}");
            assert!(msg.contains("try -C"), "got: {msg}");
            assert!(
                msg.contains(&td.path().display().to_string()),
                "parent path missing from hint: {msg}"
            );
        }
        other => panic!("expected TargetIsFile, got {other:?}"),
    }
}

#[test]
fn git_repo_resolves_to_toplevel() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    let sub = td.path().join("src/foo/bar");
    fs::create_dir_all(&sub).unwrap();
    let resolved = resolve(Some(&sub), None, Path::new("/tmp")).unwrap();
    assert_eq!(resolved, td.path().canonicalize().unwrap());
}

#[test]
fn marker_walk_finds_loopr_dir() {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join(".loopr")).unwrap();
    let sub = td.path().join("src/foo");
    fs::create_dir_all(&sub).unwrap();
    let resolved = resolve(Some(&sub), None, Path::new("/tmp")).unwrap();
    assert_eq!(resolved, td.path().canonicalize().unwrap());
}

#[test]
fn bare_taskstore_dir_is_no_longer_a_marker() {
    // Pre-v0.5.13 targets could end up with a top-level `.taskstore/`
    // directory (the old `AsyncStore::open` appended `.taskstore/`
    // itself). v0.5.13 nests taskstore at `.loopr/taskstore/`, so a
    // bare `.taskstore/` at the root is either legacy or unrelated
    // tooling and must not resolve as a loopr target.
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join(".taskstore")).unwrap();
    let sub = td.path().join("src/foo");
    fs::create_dir_all(&sub).unwrap();
    let resolved = resolve(Some(&sub), None, Path::new("/tmp")).unwrap();
    assert_eq!(
        resolved,
        sub.canonicalize().unwrap(),
        "bare .taskstore/ must fall through to the canonical start path, not anchor to the ancestor that contains it"
    );
}

#[test]
fn fall_through_to_start_when_nothing_found() {
    let td = TempDir::new().unwrap();
    let resolved = resolve(Some(td.path()), None, Path::new("/tmp")).unwrap();
    assert_eq!(resolved, td.path().canonicalize().unwrap());
}
