#![allow(clippy::unwrap_used)]

use std::fs;

use tempfile::TempDir;

use super::*;

fn target_with_loopr_dir() -> TempDir {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join(".loopr")).unwrap();
    td
}

#[test]
fn read_pid_missing_returns_none() {
    let td = TempDir::new().unwrap();
    let p = td.path().join("missing.pid");
    assert!(read_pid(&p).unwrap().is_none());
}

#[test]
fn write_pid_then_read_roundtrip() {
    let td = target_with_loopr_dir();
    let p = pid_path(td.path());
    write_pid(&p, 12345).unwrap();
    assert_eq!(read_pid(&p).unwrap(), Some(12345));
}

#[test]
fn write_pid_fails_if_already_exists_as_lock_lost() {
    let td = target_with_loopr_dir();
    let p = pid_path(td.path());
    write_pid(&p, 1).unwrap();
    let err = write_pid(&p, 2).unwrap_err();
    assert!(matches!(err, LooprError::LockLost), "got {err:?}");
}

#[test]
fn read_pid_garbage_errors() {
    let td = target_with_loopr_dir();
    let p = pid_path(td.path());
    fs::write(&p, b"not-a-number\n").unwrap();
    let err = read_pid(&p).unwrap_err();
    assert!(matches!(err, LooprError::DaemonStartup(_)), "got {err:?}");
}

#[test]
fn version_file_roundtrip() {
    let td = target_with_loopr_dir();
    let p = version_path(td.path());
    write_version(&p, "v0.5.6-g0deadbe").unwrap();
    assert_eq!(read_version(&p).unwrap().as_deref(), Some("v0.5.6-g0deadbe"));
    assert!(version_matches(&p, "v0.5.6-g0deadbe").unwrap());
    assert!(!version_matches(&p, "v0.5.7").unwrap());
}

#[test]
fn version_missing_does_not_match() {
    let td = target_with_loopr_dir();
    let p = version_path(td.path());
    assert!(!version_matches(&p, "anything").unwrap());
}

#[test]
fn run_id_file_roundtrip() {
    let td = target_with_loopr_dir();
    let p = run_id_path(td.path());
    write_run_id(&p, "20260419-123045").unwrap();
    assert_eq!(fs::read_to_string(&p).unwrap().trim(), "20260419-123045");
}

#[test]
fn clean_removes_all_sentinels_idempotently() {
    let td = target_with_loopr_dir();
    write_pid(&pid_path(td.path()), 7777).unwrap();
    write_version(&version_path(td.path()), "v1").unwrap();
    write_run_id(&run_id_path(td.path()), "20260419-000000").unwrap();
    fs::write(socket_path(td.path()), b"dummy").unwrap();
    assert!(pid_path(td.path()).exists());
    assert!(version_path(td.path()).exists());
    assert!(run_id_path(td.path()).exists());
    assert!(socket_path(td.path()).exists());
    clean(td.path());
    assert!(!pid_path(td.path()).exists());
    assert!(!version_path(td.path()).exists());
    assert!(!run_id_path(td.path()).exists());
    assert!(!socket_path(td.path()).exists());
    // Second call: idempotent.
    clean(td.path());
}

#[test]
fn is_daemon_alive_zero_pid_is_false() {
    assert!(!is_daemon_alive(0));
}

#[test]
fn is_daemon_alive_rejects_non_loopr_process() {
    // PID 1 (init) exists on every Linux host but is never named "loopr".
    assert!(!is_daemon_alive(1));
}

#[test]
fn atomic_claim_race_only_one_winner() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let td = target_with_loopr_dir();
    let p = Arc::new(pid_path(td.path()));
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for i in 0..8 {
        let p = p.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            write_pid(&p, 1000 + i as u32)
        }));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    let losses = results
        .iter()
        .filter(|r| matches!(r, Err(LooprError::LockLost)))
        .count();
    assert_eq!(wins, 1, "exactly one winner expected");
    assert_eq!(losses, 7, "seven losers expected");
}
