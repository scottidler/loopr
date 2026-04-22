//! Tests for `ensure_loopr_excludes`. Uses `tempfile` + a fabricated
//! `.git/` directory — we don't need a real git repo, just the
//! `info/exclude` hierarchy.

#![allow(clippy::unwrap_used)]

use super::*;

fn set_up() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    // Fabricate `.git/info/` so we don't need `git init`.
    std::fs::create_dir_all(tmp.path().join(".git").join("info")).unwrap();
    tmp
}

#[test]
fn writes_marker_and_patterns_on_fresh_file() {
    let tmp = set_up();
    ensure_loopr_excludes(tmp.path()).unwrap();

    let body = std::fs::read_to_string(tmp.path().join(".git").join("info").join("exclude")).unwrap();
    assert!(body.contains(LOOPR_EXCLUDE_MARKER));
    assert!(body.contains(".loopr/runs/"));
    assert!(body.contains(".loopr/worktrees/"));
    assert!(body.contains(".loopr/socket"));
    assert!(body.contains(".loopr/daemon.pid"));
    assert!(body.contains(".loopr/config.yml"));
    // Taskstore is committed per vision; must NOT be excluded.
    assert!(!body.contains(".loopr/taskstore"));
}

#[test]
fn appends_to_existing_file_preserving_prior_content() {
    let tmp = set_up();
    let exclude = tmp.path().join(".git").join("info").join("exclude");
    std::fs::write(&exclude, "# user pattern\nmy-tmp/\n").unwrap();

    ensure_loopr_excludes(tmp.path()).unwrap();

    let body = std::fs::read_to_string(&exclude).unwrap();
    assert!(body.contains("# user pattern"));
    assert!(body.contains("my-tmp/"));
    assert!(body.contains(LOOPR_EXCLUDE_MARKER));
}

#[test]
fn is_idempotent_when_marker_already_present() {
    let tmp = set_up();
    ensure_loopr_excludes(tmp.path()).unwrap();
    let first = std::fs::read_to_string(tmp.path().join(".git").join("info").join("exclude")).unwrap();
    ensure_loopr_excludes(tmp.path()).unwrap();
    let second = std::fs::read_to_string(tmp.path().join(".git").join("info").join("exclude")).unwrap();
    assert_eq!(first, second, "second call must not change anything");
}
