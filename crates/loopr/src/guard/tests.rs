#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::*;

#[test]
fn no_sentinel_anywhere_returns_ok() {
    let td = TempDir::new().unwrap();
    assert!(check(td.path()).is_ok());
}

#[test]
fn sentinel_at_start_path_trips() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join(SENTINEL), "").unwrap();
    let err = check(td.path()).unwrap_err();
    match err {
        LooprError::SourceGuardTripped { path, sentinel } => {
            assert_eq!(path, td.path());
            assert_eq!(sentinel, td.path().join(SENTINEL));
        }
        other => panic!("expected SourceGuardTripped, got {other:?}"),
    }
}

#[test]
fn sentinel_at_ancestor_trips() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join(SENTINEL), "").unwrap();
    let subdir = td.path().join("a/b/c");
    fs::create_dir_all(&subdir).unwrap();
    let err = check(&subdir).unwrap_err();
    match err {
        LooprError::SourceGuardTripped { path, sentinel } => {
            assert_eq!(path, subdir);
            assert_eq!(sentinel, td.path().join(SENTINEL));
        }
        other => panic!("expected SourceGuardTripped, got {other:?}"),
    }
}

#[test]
fn sentinel_in_sibling_does_not_trip() {
    let td = TempDir::new().unwrap();
    let sibling = td.path().join("sibling");
    let target = td.path().join("target");
    fs::create_dir_all(&sibling).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::write(sibling.join(SENTINEL), "").unwrap();
    assert!(check(&target).is_ok());
}

#[test]
fn tmp_is_clean() {
    // /tmp itself should never carry the sentinel on a sane machine.
    assert!(check(Path::new("/tmp")).is_ok());
}
