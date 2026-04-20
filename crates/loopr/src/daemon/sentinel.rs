//! Filesystem sentinel helpers for the daemon lifecycle.
//!
//! The daemon owns four sentinel files under `<target>/.loopr/`:
//!
//! * `daemon.pid` - the daemon's PID, written with `O_CREAT | O_EXCL` (atomic claim)
//! * `daemon.version` - the exact `GIT_DESCRIBE` string of the binary that forked the daemon (for silent-restart on version drift)
//! * `daemon.run-id` - the daemon's own `RunId` (so clients and tests can locate the daemon's log dir without a round-trip)
//! * `socket` - the Unix domain socket
//!
//! Everything here is sync; no tokio. Callers that want to wait for the
//! daemon to become ready live in the transport layer.

// Phase 2 ships the sentinel helpers in isolation. Phase 3 wires them into
// `daemon::ensure_daemon` / `daemon_main`; the `allow` below is the
// "active transition" exception to the usual `#![deny(dead_code)]` rule,
// and is removed in Phase 3 once every helper is consumed.
#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::LooprError;

/// SIGTERM escalation window used by `kill_stale` and `stop` flows before
/// we escalate to SIGKILL.
const STOP_TIMEOUT_SECS: u64 = 3;
const STOP_POLL_INTERVAL_MS: u64 = 50;

/// Filename of the PID file under `<target>/.loopr/`.
pub const PID_FILENAME: &str = "daemon.pid";
/// Filename of the daemon-version file.
pub const VERSION_FILENAME: &str = "daemon.version";
/// Filename of the daemon-run-id pointer.
pub const RUN_ID_FILENAME: &str = "daemon.run-id";
/// Filename of the Unix domain socket.
pub const SOCKET_FILENAME: &str = "socket";

/// Path to `<target>/.loopr/daemon.pid`.
pub fn pid_path(target: &Path) -> PathBuf {
    target.join(".loopr").join(PID_FILENAME)
}

/// Path to `<target>/.loopr/daemon.version`.
pub fn version_path(target: &Path) -> PathBuf {
    target.join(".loopr").join(VERSION_FILENAME)
}

/// Path to `<target>/.loopr/daemon.run-id`.
pub fn run_id_path(target: &Path) -> PathBuf {
    target.join(".loopr").join(RUN_ID_FILENAME)
}

/// Path to `<target>/.loopr/socket`.
pub fn socket_path(target: &Path) -> PathBuf {
    target.join(".loopr").join(SOCKET_FILENAME)
}

/// Read the PID from a pid file. `Ok(None)` means the file does not exist;
/// `Err` is a real I/O or parse failure.
pub fn read_pid(pid_file: &Path) -> Result<Option<u32>, LooprError> {
    match fs::read_to_string(pid_file) {
        Ok(s) => {
            let trimmed = s.trim();
            trimmed
                .parse::<u32>()
                .map(Some)
                .map_err(|e| LooprError::DaemonStartup(format!("bad pid in {}: {e}", pid_file.display())))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LooprError::DaemonStartup(format!("read {}: {e}", pid_file.display()))),
    }
}

/// Atomically claim the PID file. Uses `O_CREAT | O_EXCL` so concurrent
/// grandchildren cannot both win. Returns `LooprError::LockLost` if the
/// file already exists.
pub fn write_pid(pid_file: &Path, pid: u32) -> Result<(), LooprError> {
    if let Some(parent) = pid_file.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| LooprError::DaemonStartup(format!("mkdir {}: {e}", parent.display())))?;
    }
    let mut f = match OpenOptions::new().write(true).create_new(true).open(pid_file) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Err(LooprError::LockLost),
        Err(e) => {
            return Err(LooprError::DaemonStartup(format!(
                "open {} for exclusive write: {e}",
                pid_file.display()
            )));
        }
    };
    writeln!(f, "{pid}").map_err(|e| LooprError::DaemonStartup(format!("write {}: {e}", pid_file.display())))?;
    Ok(())
}

/// Write the daemon-version file. Overwrites any existing content.
pub fn write_version(version_file: &Path, version: &str) -> Result<(), LooprError> {
    if let Some(parent) = version_file.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| LooprError::DaemonStartup(format!("mkdir {}: {e}", parent.display())))?;
    }
    fs::write(version_file, format!("{version}\n"))
        .map_err(|e| LooprError::DaemonStartup(format!("write {}: {e}", version_file.display())))
}

/// Read the daemon-version file. `Ok(None)` means the file does not exist.
pub fn read_version(version_file: &Path) -> Result<Option<String>, LooprError> {
    match fs::read_to_string(version_file) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LooprError::DaemonStartup(format!(
            "read {}: {e}",
            version_file.display()
        ))),
    }
}

/// Write the daemon-run-id file. Overwrites any existing content.
pub fn write_run_id(run_id_file: &Path, run_id: &str) -> Result<(), LooprError> {
    if let Some(parent) = run_id_file.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| LooprError::DaemonStartup(format!("mkdir {}: {e}", parent.display())))?;
    }
    fs::write(run_id_file, format!("{run_id}\n"))
        .map_err(|e| LooprError::DaemonStartup(format!("write {}: {e}", run_id_file.display())))
}

/// Return `true` iff the stored version string equals `binary_version`.
/// Missing file = no match (triggers a fresh fork).
pub fn version_matches(version_file: &Path, binary_version: &str) -> Result<bool, LooprError> {
    Ok(read_version(version_file)?.as_deref() == Some(binary_version))
}

/// Process-liveness check. Two-step:
///   1. `kill(pid, 0)` probes liveness without delivering a signal.
///   2. On success, read the process name (`/proc/<pid>/comm` on Linux,
///      `ps -p <pid> -o comm=` as a portable fallback) and verify it is
///      `loopr`. This guards against PID reuse -- another process could
///      have been assigned the PID after the daemon died.
///
/// Returns `false` on any error (treat "couldn't verify" as "not alive"
/// so the caller cleans up and re-forks).
pub fn is_daemon_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Step 1: kill(pid, 0)
    // SAFETY: kill with signal 0 only probes the PID, it does not deliver
    // a signal. No memory or lifetime concerns.
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if alive != 0 {
        return false;
    }
    // Step 2: verify the process name is `loopr`.
    process_name_is_loopr(pid)
}

/// Read the process name for a PID and return `true` iff it is `loopr`.
/// Linux-first via `/proc/<pid>/comm`; falls back to `ps -p <pid> -o comm=`
/// for portability (macOS / container images without procfs).
fn process_name_is_loopr(pid: u32) -> bool {
    let procfs = PathBuf::from(format!("/proc/{pid}/comm"));
    if let Ok(mut f) = fs::File::open(&procfs) {
        let mut s = String::new();
        if f.read_to_string(&mut s).is_ok() {
            return s.trim() == "loopr";
        }
    }
    // Portable fallback.
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let name = String::from_utf8_lossy(&out.stdout);
            let last = name.trim().rsplit('/').next().unwrap_or("").trim();
            last == "loopr"
        }
        _ => false,
    }
}

/// Send SIGTERM to the PID in `pid_file`, poll for exit up to
/// `STOP_TIMEOUT_SECS`, escalate to SIGKILL on timeout. Removes the
/// sentinel files afterward. No-op if no daemon is running.
pub fn kill_stale(target: &Path) -> Result<(), LooprError> {
    let pid_file = pid_path(target);
    let pid = match read_pid(&pid_file)? {
        Some(p) => p,
        None => return Ok(()),
    };
    if !is_daemon_alive(pid) {
        clean(target);
        return Ok(());
    }
    // SAFETY: kill(pid, SIGTERM) delivers a signal to the PID. The PID
    // was name-verified as `loopr` above.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(STOP_TIMEOUT_SECS);
    while Instant::now() < deadline {
        if !is_daemon_alive(pid) {
            clean(target);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(STOP_POLL_INTERVAL_MS));
    }
    // SIGKILL escalation.
    // SAFETY: same as SIGTERM above.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    // Give the kernel a beat to reap.
    std::thread::sleep(Duration::from_millis(STOP_POLL_INTERVAL_MS));
    clean(target);
    Ok(())
}

/// Remove all four sentinel files. Idempotent: missing files are not an
/// error. Used by the graceful daemon shutdown path and by `kill_stale`'s
/// fallback.
pub fn clean(target: &Path) {
    for path in [
        pid_path(target),
        version_path(target),
        run_id_path(target),
        socket_path(target),
    ] {
        let _ = fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests;
