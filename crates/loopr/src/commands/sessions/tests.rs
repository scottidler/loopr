#![allow(clippy::unwrap_used)]

use std::fs;

use tempfile::TempDir;

use crate::cli::SessionsCmd;
use crate::commands::sessions;
use crate::session;

/// Tests here share process-global `$XDG_DATA_HOME` with the resolver
/// tests (`session/tests.rs`); delegate to the crate-shared mutex so
/// both modules serialize against the same lock.
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    crate::session::test_env_mutex()
}

fn with_xdg_home<F: FnOnce(&std::path::Path)>(f: F) {
    let td = TempDir::new().unwrap();
    let prev = std::env::var("XDG_DATA_HOME").ok();
    unsafe { std::env::set_var("XDG_DATA_HOME", td.path()) };
    f(td.path());
    match prev {
        Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
    }
}

#[test]
fn new_allocates_session_and_claims_pointer() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let id = session::allocate_new(target.path()).unwrap();
        // Pointer should name the new id.
        let content = fs::read_to_string(session::pointer_path(target.path())).unwrap();
        assert_eq!(content.trim(), id.as_str());
    });
}

#[test]
fn end_active_marks_manifest_and_clears_pointer() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let id = session::allocate_new(target.path()).unwrap();
        let ended = session::end_active(target.path()).unwrap();
        assert_eq!(ended.unwrap().as_str(), id.as_str());
        assert!(!session::pointer_path(target.path()).exists(), "pointer cleared");
        let manifest = session::read_manifest(&id).unwrap();
        assert!(manifest.ended_at.is_some(), "ended_at set");
    });
}

#[test]
fn end_active_on_absent_returns_none() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let r = session::end_active(target.path()).unwrap();
        assert!(r.is_none());
    });
}

#[test]
fn list_all_returns_entries_sorted() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let a = session::allocate_new(target.path()).unwrap();
        let b = session::allocate_new(target.path()).unwrap();
        let c = session::allocate_new(target.path()).unwrap();
        let listed: Vec<String> = session::list_all()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id.as_str().to_string())
            .collect();
        let mut expected = vec![a.as_str().to_string(), b.as_str().to_string(), c.as_str().to_string()];
        expected.sort();
        assert_eq!(listed, expected);
    });
}

#[test]
fn list_all_empty_on_no_sessions_dir() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let entries = session::list_all().unwrap();
        assert!(entries.is_empty());
    });
}

#[test]
fn list_all_skips_malformed_dir_names() {
    let _g = env_guard();
    with_xdg_home(|xdg_dir| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        session::allocate_new(target.path()).unwrap();
        // Create a stray dir that is not a valid session-id.
        fs::create_dir_all(xdg_dir.join("loopr").join("sessions").join("not-a-session-id")).unwrap();
        // list_all returns only the valid session.
        let entries = session::list_all().unwrap();
        assert_eq!(entries.len(), 1);
    });
}

#[test]
fn read_active_returns_none_when_pointer_absent() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let r = session::read_active(target.path()).unwrap();
        assert!(r.is_none());
    });
}

#[test]
fn read_active_returns_none_when_pointer_corrupt() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        fs::write(session::pointer_path(target.path()), "garbage\n").unwrap();
        let r = session::read_active(target.path()).unwrap();
        assert!(r.is_none());
    });
}

#[test]
fn session_processes_walks_xdg_runs_dir() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let id = session::allocate_new(target.path()).unwrap();
        // Manually scaffold two process dirs under two target slugs.
        let base = telemetry::session_dir(&id).unwrap().join("targets");
        fs::create_dir_all(base.join("slug-a").join("runs").join("pc-aaaaaa")).unwrap();
        fs::create_dir_all(base.join("slug-a").join("runs").join("pc-bbbbbb")).unwrap();
        fs::create_dir_all(base.join("slug-b").join("runs").join("pc-cccccc")).unwrap();
        let procs = session::session_processes(&id).unwrap();
        assert_eq!(procs.len(), 2, "two target slugs");
        assert_eq!(procs[0].0, "slug-a");
        assert_eq!(procs[0].1.len(), 2);
        assert_eq!(procs[1].0, "slug-b");
        assert_eq!(procs[1].1.len(), 1);
    });
}

#[test]
fn run_status_prints_no_active_when_absent() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        // `run` dispatches to `status`; its stdout printing is not
        // captured by the test harness, but we can at least verify no
        // error is returned (the "no active session" branch).
        sessions::run(target.path(), SessionsCmd::Status).unwrap();
    });
}

#[test]
fn run_list_empty_does_not_error() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        sessions::run(target.path(), SessionsCmd::List).unwrap();
    });
}

#[test]
fn run_lifecycle_new_status_end_list() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        sessions::run(target.path(), SessionsCmd::New).unwrap();
        sessions::run(target.path(), SessionsCmd::Status).unwrap();
        sessions::run(target.path(), SessionsCmd::End).unwrap();
        // After End, pointer is gone.
        assert!(!session::pointer_path(target.path()).exists());
        sessions::run(target.path(), SessionsCmd::List).unwrap();
    });
}

#[test]
fn run_resume_attaches_existing_session() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let id = session::allocate_new(target.path()).unwrap();
        // Clear pointer so resume has work to do.
        fs::remove_file(session::pointer_path(target.path())).unwrap();
        sessions::run(
            target.path(),
            SessionsCmd::Resume {
                id: id.as_str().to_string(),
            },
        )
        .unwrap();
        let content = fs::read_to_string(session::pointer_path(target.path())).unwrap();
        assert_eq!(content.trim(), id.as_str());
    });
}

#[test]
fn run_resume_rejects_malformed_id() {
    let _g = env_guard();
    with_xdg_home(|_| {
        let target = TempDir::new().unwrap();
        fs::create_dir_all(target.path().join(".loopr")).unwrap();
        let err = sessions::run(
            target.path(),
            SessionsCmd::Resume {
                id: "not-a-session-id".to_string(),
            },
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bad --session") || msg.contains("not-a-session-id"));
    });
}
