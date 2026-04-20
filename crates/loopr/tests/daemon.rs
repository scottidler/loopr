//! Stage 4 Phase 6 end-to-end tests. Exercises the compiled `loopr`
//! binary against a live forked daemon in a `TempDir`; every test uses
//! its own target, starts/stops its own daemon, and asserts the
//! published acceptance criteria (see `crates/loopr/docs/design/
//! 2026-04-19-daemon-stage-4.md`).

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn loopr() -> Command {
    Command::cargo_bin("loopr").unwrap()
}

fn read_pid(target: &Path) -> Option<u32> {
    fs::read_to_string(target.join(".loopr").join("daemon.pid"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) probes liveness without delivering a signal.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn stop_daemon(target: &Path) {
    let Some(pid) = read_pid(target) else { return };
    // SAFETY: user-owned PID; worst case the process is gone.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !pid_is_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

fn start_daemon(target: &Path) {
    loopr()
        .args(["-C", target.to_str().unwrap(), "daemon", "start"])
        .assert()
        .success();
    // Wait for sentinel files to appear so subsequent commands see a
    // fully-formed daemon.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if target.join(".loopr").join("socket").exists() && read_pid(target).is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not become ready in 3s");
}

// AC 6: `loopr -C /tmp daemon status` connects, handshakes, receives a
// StatusResult, prints it in a human-readable form (key: value lines),
// exits 0.
#[test]
fn ac6_daemon_status_prints_human_readable() {
    let td = TempDir::new().unwrap();
    start_daemon(td.path());

    let output = loopr()
        .args(["-C", td.path().to_str().unwrap(), "daemon", "status"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).to_string();
    assert!(stdout.contains("pid:"), "pid line: {stdout}");
    assert!(stdout.contains("started-at:"), "started-at line: {stdout}");
    assert!(stdout.contains("active-plans:"), "active-plans line: {stdout}");
    assert!(stdout.contains("active-works:"), "active-works line: {stdout}");

    stop_daemon(td.path());
}

// AC 7: `loopr daemon stop` sends SIGTERM; daemon exits; sentinel files
// removed.
#[test]
fn ac7_daemon_stop_removes_sentinels() {
    let td = TempDir::new().unwrap();
    start_daemon(td.path());
    let pid = read_pid(td.path()).unwrap();

    loopr()
        .args(["-C", td.path().to_str().unwrap(), "daemon", "stop"])
        .assert()
        .success();

    // Sentinels cleaned.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if read_pid(td.path()).is_none() && !td.path().join(".loopr").join("socket").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(read_pid(td.path()).is_none(), "pid file removed");
    assert!(!td.path().join(".loopr").join("socket").exists(), "socket removed");
    assert!(
        !td.path().join(".loopr").join("daemon.version").exists(),
        "version file removed"
    );
    assert!(
        !td.path().join(".loopr").join("daemon.run-id").exists(),
        "run-id file removed"
    );

    // Process actually exited.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && pid_is_alive(pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_is_alive(pid), "daemon process exited");
}

// AC 8: `loopr daemon stop` with no daemon running prints "no daemon
// running" and exits 0.
#[test]
fn ac8_daemon_stop_no_daemon_prints_message() {
    let td = TempDir::new().unwrap();
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "daemon", "stop"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no daemon running"));
}

// AC 9: `loopr plan "x"` with no daemon auto-forks, round-trips, returns
// Stage 5. Verifies the daemon was actually forked (its pid exists).
#[test]
fn ac9_plan_auto_forks_daemon_then_stage5() {
    let td = TempDir::new().unwrap();
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 5"));
    assert!(read_pid(td.path()).is_some(), "daemon was auto-forked");

    stop_daemon(td.path());
}

// AC 10: second `plan` invocation reuses the running daemon; the pid
// does NOT change.
#[test]
fn ac10_plan_reuses_running_daemon() {
    let td = TempDir::new().unwrap();
    start_daemon(td.path());
    let pid_before = read_pid(td.path()).unwrap();

    loopr()
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 5"));
    let pid_after = read_pid(td.path()).unwrap();
    assert_eq!(pid_before, pid_after, "same daemon reused");

    stop_daemon(td.path());
}

// AC 11: a second `daemon start` while one is running is idempotent --
// no re-fork, pid unchanged, prints "daemon already running".
#[test]
fn ac11_second_daemon_start_is_idempotent() {
    let td = TempDir::new().unwrap();
    start_daemon(td.path());
    let pid_before = read_pid(td.path()).unwrap();

    let output = loopr()
        .args(["-C", td.path().to_str().unwrap(), "daemon", "start"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).to_string();
    assert!(
        stdout.contains("daemon already running"),
        "idempotent message: {stdout}"
    );
    let pid_after = read_pid(td.path()).unwrap();
    assert_eq!(pid_before, pid_after, "no re-fork");

    stop_daemon(td.path());
}

// AC 12: stale pid (PID in file no longer alive): cleanup + fresh fork.
#[test]
fn ac12_stale_pid_triggers_cleanup_and_fresh_fork() {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join(".loopr")).unwrap();
    // Use a PID that is almost certainly not alive and not named `loopr`.
    fs::write(td.path().join(".loopr").join("daemon.pid"), "999999\n").unwrap();

    start_daemon(td.path());
    let new_pid = read_pid(td.path()).unwrap();
    assert_ne!(new_pid, 999999, "stale pid replaced");
    assert!(pid_is_alive(new_pid), "fresh daemon alive");

    stop_daemon(td.path());
}

// AC 13: `.loopr/daemon.version` mismatches the binary's GIT_DESCRIBE:
// SIGTERM old daemon, fork fresh (silent restart). We simulate by
// starting a daemon, overwriting the version file, then running another
// client command and asserting the pid changed.
#[test]
fn ac13_version_mismatch_triggers_silent_restart() {
    let td = TempDir::new().unwrap();
    start_daemon(td.path());
    let pid_before = read_pid(td.path()).unwrap();
    // Overwrite the version file with a bogus value so the next client
    // command sees a version mismatch.
    fs::write(td.path().join(".loopr").join("daemon.version"), "not-a-real-version\n").unwrap();

    loopr()
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 5"));
    let pid_after = read_pid(td.path()).unwrap();
    assert_ne!(pid_before, pid_after, "daemon was restarted");

    stop_daemon(td.path());
}

// AC 15: a PID file pointing at a non-loopr process is treated as stale
// (because is_daemon_alive's name check rejects it). ensure_daemon_if_
// needed cleans the sentinel and forks a fresh daemon. We use PID 1
// (init) which exists on every Linux host but is never named loopr.
#[test]
fn ac15_pid_reuse_protection_rejects_non_loopr() {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join(".loopr")).unwrap();
    fs::write(td.path().join(".loopr").join("daemon.pid"), "1\n").unwrap();

    // Use a command that triggers ensure_daemon_if_needed.
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "daemon", "status"])
        .assert()
        .success();

    let pid_after = read_pid(td.path()).unwrap();
    assert_ne!(pid_after, 1, "stale PID 1 replaced with a freshly-forked daemon");
    assert!(pid_is_alive(pid_after), "fresh daemon is alive at pid {pid_after}");

    stop_daemon(td.path());
}

// AC 22: cargo-tree on loopr shows tokio / tokio-util / libc (and the
// other Stage 4 deps added via cargo add).
#[test]
fn ac22_loopr_depends_on_tokio_tokio_util_libc() {
    let output = Command::new("cargo")
        .args(["tree", "-p", "loopr", "-e", "normal,build", "--prefix", "none"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("tokio "), "tokio in tree: {stdout}");
    assert!(stdout.contains("tokio-util "), "tokio-util in tree: {stdout}");
    assert!(stdout.contains("libc "), "libc in tree: {stdout}");
}

// AC 21: `ipc` crate remains I/O-free -- tokio / tokio-util / runtime
// deps must not appear in its transitive tree.
#[test]
fn ac21_ipc_is_io_free() {
    let output = Command::new("cargo")
        .args(["tree", "-p", "ipc", "-e", "normal,build", "--prefix", "none"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("tokio "), "ipc must not depend on tokio: {stdout}");
    assert!(
        !stdout.contains("tokio-util "),
        "ipc must not depend on tokio-util: {stdout}"
    );
}
