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

mod common;
use common::{DaemonAutoStop, init_git_repo};

/// Build a `loopr` subprocess with `XDG_DATA_HOME` pointed at a
/// test-local dir so session state doesn't pollute the real user's
/// `~/.local/share/loopr/`.
fn loopr(target: &Path) -> Command {
    let mut cmd = Command::cargo_bin("loopr").unwrap();
    cmd.env("XDG_DATA_HOME", xdg_home_for(target));
    cmd
}

fn xdg_home_for(target: &Path) -> std::path::PathBuf {
    target.join(".xdg")
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
    fs::create_dir_all(xdg_home_for(target)).unwrap();
    loopr(target)
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

// AC 3: `.loopr/daemon.version` must equal the binary's `GIT_DESCRIBE`.
// This is the pivot that lets a client detect a version-mismatched
// daemon and trigger a silent restart.
#[test]
fn ac3_version_file_matches_git_describe() {
    let td = TempDir::new().unwrap();
    let _stop = DaemonAutoStop::for_target(td.path());
    start_daemon(td.path());

    let written = fs::read_to_string(td.path().join(".loopr").join("daemon.version")).unwrap();
    // The test binary and the loopr binary share the same workspace
    // package version, so `env!("GIT_DESCRIBE")` in the test resolves
    // to the same string `daemon.rs` wrote. Trim trailing newline.
    assert_eq!(written.trim(), env!("GIT_DESCRIBE"), "version file content");

    stop_daemon(td.path());
}

// AC 4: `.loopr/daemon.process-id` must name an existing process dir
// under XDG. Log queries rely on this pointer plus the active-session
// pointer to locate the daemon's own run dir.
#[test]
fn ac4_process_id_file_points_to_extant_run_dir() {
    let td = TempDir::new().unwrap();
    let _stop = DaemonAutoStop::for_target(td.path());
    start_daemon(td.path());

    let session_id = fs::read_to_string(td.path().join(".loopr").join("active-session")).unwrap();
    let session_id = session_id.trim();
    let process_id = fs::read_to_string(td.path().join(".loopr").join("daemon.process-id")).unwrap();
    let process_id = process_id.trim();
    let slug = td.path().to_str().unwrap().replace('/', "-");
    let run_dir = xdg_home_for(td.path())
        .join("loopr")
        .join("sessions")
        .join(session_id)
        .join("targets")
        .join(&slug)
        .join("runs")
        .join(process_id);
    assert!(
        run_dir.is_dir(),
        "daemon.process-id ({process_id}) points to extant dir: {}",
        run_dir.display()
    );

    stop_daemon(td.path());
}

// AC 5: the daemon allocates its own telemetry guard and emits at least
// one `daemon.started` event to its run dir's events.log under XDG.
#[test]
fn ac5_daemon_emits_started_event_to_its_own_events_log() {
    let td = TempDir::new().unwrap();
    let _stop = DaemonAutoStop::for_target(td.path());
    start_daemon(td.path());

    let session_id = fs::read_to_string(td.path().join(".loopr").join("active-session"))
        .unwrap()
        .trim()
        .to_string();
    let process_id = fs::read_to_string(td.path().join(".loopr").join("daemon.process-id"))
        .unwrap()
        .trim()
        .to_string();
    let slug = td.path().to_str().unwrap().replace('/', "-");
    let run_dir = xdg_home_for(td.path())
        .join("loopr")
        .join("sessions")
        .join(&session_id)
        .join("targets")
        .join(&slug)
        .join("runs")
        .join(&process_id);
    let events = run_dir.join("events.log");
    let pretty = run_dir.join("loopr.log");

    // Give the tracing-appender a beat to flush the first event.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if events.is_file() && fs::metadata(&events).map(|m| m.len() > 0).unwrap_or(false) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let events_body = fs::read_to_string(&events).unwrap();
    assert!(
        events_body.contains("daemon.started"),
        "events.log contains daemon.started: {events_body}"
    );
    assert!(pretty.is_file(), "pretty log exists: {}", pretty.display());

    stop_daemon(td.path());
}

// AC 14: `loopr daemon start --foreground` while a background daemon is
// already running must exit non-zero with a clear error that mentions
// the running pid and directs the user at `loopr daemon stop`.
#[test]
fn ac14_foreground_start_blocked_by_running_background_daemon() {
    let td = TempDir::new().unwrap();
    let _stop = DaemonAutoStop::for_target(td.path());
    start_daemon(td.path());
    let pid = read_pid(td.path()).unwrap();

    let assertion = loopr(td.path())
        .args(["-C", td.path().to_str().unwrap(), "daemon", "start", "--foreground"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        stderr.contains("daemon already running"),
        "stderr mentions collision: {stderr}"
    );
    assert!(
        stderr.contains(&pid.to_string()),
        "stderr mentions the running pid {pid}: {stderr}"
    );
    assert!(
        stderr.contains("loopr daemon stop"),
        "stderr hints at the fix: {stderr}"
    );

    stop_daemon(td.path());
}

// AC 6: `loopr -C /tmp daemon status` connects, handshakes, receives a
// StatusResult, prints it in a human-readable form (key: value lines),
// exits 0.
#[test]
fn ac6_daemon_status_prints_human_readable() {
    let td = TempDir::new().unwrap();
    let _stop = DaemonAutoStop::for_target(td.path());
    start_daemon(td.path());

    let output = loopr(td.path())
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
    let _stop = DaemonAutoStop::for_target(td.path());
    start_daemon(td.path());
    let pid = read_pid(td.path()).unwrap();

    loopr(td.path())
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
        !td.path().join(".loopr").join("daemon.process-id").exists(),
        "process-id file removed"
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
    let _stop = DaemonAutoStop::for_target(td.path());
    loopr(td.path())
        .args(["-C", td.path().to_str().unwrap(), "daemon", "stop"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no daemon running"));
}

// AC 9: `loopr plan "x"` with no daemon auto-forks, round-trips, and
// prints the created plan on success. Verifies the daemon was actually
// forked (its pid exists). Stage 5 upgrade: `plan` now succeeds instead
// of returning the Stage-4 StageUnimplemented stub.
#[test]
fn ac9_plan_auto_forks_daemon_and_creates_plan() {
    let td = TempDir::new().unwrap();
    let _stop = DaemonAutoStop::for_target(td.path());
    init_git_repo(td.path());
    loopr(td.path())
        .args(["-C", td.path().to_str().unwrap(), "plan", "create", "x"])
        .assert()
        .success()
        .stdout(predicate::str::contains("plan:"));
    assert!(read_pid(td.path()).is_some(), "daemon was auto-forked");

    stop_daemon(td.path());
}

// AC 10: second `plan` invocation reuses the running daemon; the pid
// does NOT change.
#[test]
fn ac10_plan_reuses_running_daemon() {
    let td = TempDir::new().unwrap();
    let _stop = DaemonAutoStop::for_target(td.path());
    init_git_repo(td.path());
    start_daemon(td.path());
    let pid_before = read_pid(td.path()).unwrap();

    loopr(td.path())
        .args(["-C", td.path().to_str().unwrap(), "plan", "create", "x"])
        .assert()
        .success();
    let pid_after = read_pid(td.path()).unwrap();
    assert_eq!(pid_before, pid_after, "same daemon reused");

    stop_daemon(td.path());
}

// AC 11: a second `daemon start` while one is running is idempotent --
// no re-fork, pid unchanged, prints "daemon already running".
#[test]
fn ac11_second_daemon_start_is_idempotent() {
    let td = TempDir::new().unwrap();
    let _stop = DaemonAutoStop::for_target(td.path());
    start_daemon(td.path());
    let pid_before = read_pid(td.path()).unwrap();

    let output = loopr(td.path())
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
    let _stop = DaemonAutoStop::for_target(td.path());
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
    let _stop = DaemonAutoStop::for_target(td.path());
    init_git_repo(td.path());
    start_daemon(td.path());
    let pid_before = read_pid(td.path()).unwrap();
    // Overwrite the version file with a bogus value so the next client
    // command sees a version mismatch.
    fs::write(td.path().join(".loopr").join("daemon.version"), "not-a-real-version\n").unwrap();

    loopr(td.path())
        .args(["-C", td.path().to_str().unwrap(), "plan", "create", "x"])
        .assert()
        .success();
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
    let _stop = DaemonAutoStop::for_target(td.path());
    fs::create_dir_all(td.path().join(".loopr")).unwrap();
    fs::write(td.path().join(".loopr").join("daemon.pid"), "1\n").unwrap();

    // Use a command that triggers ensure_daemon_if_needed.
    loopr(td.path())
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
