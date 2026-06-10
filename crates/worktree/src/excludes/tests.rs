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

fn exclude_path(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join(".git").join("info").join("exclude")
}

#[test]
fn writes_marker_and_patterns_on_fresh_file() {
    let tmp = set_up();
    ensure_loopr_excludes(tmp.path()).unwrap();

    let body = std::fs::read_to_string(exclude_path(&tmp)).unwrap();
    assert!(body.contains(LOOPR_EXCLUDE_MARKER));
    for pattern in LOOPR_EXCLUDES {
        assert!(body.contains(pattern), "missing {pattern}");
    }
    // Stale pattern dropped in Phase 8; the daemon glob replaces daemon.pid.
    assert!(!body.contains(".loopr/runs/"));
    assert!(body.contains(".loopr/daemon.*"));
    // Taskstore is committed per vision; must NOT be excluded.
    assert!(!body.contains(".loopr/taskstore"));
}

#[test]
fn appends_to_existing_file_preserving_prior_content() {
    let tmp = set_up();
    let exclude = exclude_path(&tmp);
    std::fs::write(&exclude, "# user pattern\nmy-tmp/\n").unwrap();

    ensure_loopr_excludes(tmp.path()).unwrap();

    let body = std::fs::read_to_string(&exclude).unwrap();
    assert!(body.contains("# user pattern"));
    assert!(body.contains("my-tmp/"));
    assert!(body.contains(LOOPR_EXCLUDE_MARKER));
}

#[test]
fn is_idempotent_when_all_patterns_present() {
    let tmp = set_up();
    ensure_loopr_excludes(tmp.path()).unwrap();
    let first = std::fs::read_to_string(exclude_path(&tmp)).unwrap();
    ensure_loopr_excludes(tmp.path()).unwrap();
    let second = std::fs::read_to_string(exclude_path(&tmp)).unwrap();
    assert_eq!(first, second, "second call must not change anything");
}

#[test]
fn grows_list_on_already_initialized_target() {
    // Simulate an old target initialized with a marker + a SUBSET of the
    // current patterns. A re-run must append the now-missing patterns
    // (per-pattern growth), not skip on seeing the marker.
    let tmp = set_up();
    let exclude = exclude_path(&tmp);
    std::fs::write(
        &exclude,
        format!("{LOOPR_EXCLUDE_MARKER}\n.loopr/worktrees/\n.loopr/socket\n"),
    )
    .unwrap();

    ensure_loopr_excludes(tmp.path()).unwrap();

    let body = std::fs::read_to_string(&exclude).unwrap();
    // The previously-missing records/ pattern must now be present.
    assert!(body.contains(".loopr/records/"), "list did not grow: {body}");
    assert!(body.contains(".loopr/costs.jsonl"));
    // The marker is not duplicated.
    assert_eq!(body.matches(LOOPR_EXCLUDE_MARKER).count(), 1, "marker duplicated: {body}");
    // Pre-existing patterns are not re-appended.
    assert_eq!(body.matches(".loopr/worktrees/").count(), 1, "worktrees re-appended: {body}");
}

#[test]
fn unreadable_file_propagates_error_without_clobbering() {
    // A read error other than NotFound must propagate, never silently
    // clobber. We simulate by making `exclude` a directory: `read_to_string`
    // fails with an error whose kind is NOT NotFound.
    let tmp = set_up();
    let exclude = exclude_path(&tmp);
    std::fs::create_dir_all(&exclude).unwrap();

    let err = ensure_loopr_excludes(tmp.path());
    assert!(err.is_err(), "expected propagated read error, got Ok");
    // The directory we created is still there (not clobbered into a file).
    assert!(exclude.is_dir(), "exclude path was clobbered");
}
