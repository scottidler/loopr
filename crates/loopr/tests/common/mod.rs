//! Shared helpers for integration tests.
//!
//! Not every test file uses every helper, so we annotate with
//! `#[allow(dead_code)]` to silence the per-test-file unused warnings.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]

pub mod harness;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// RAII guard that SIGTERMs the daemon for `target` on Drop. Construct
/// this BEFORE any `loopr` CLI invocation that may auto-fork a daemon;
/// the Drop runs whether the test passes, fails, or panics, so daemons
/// never leak as orphans into init.
///
/// The previous pattern called `stop_daemon(target)` at the *end* of
/// each test, AFTER `assert().success()`. When an assertion panicked
/// (the common failure mode under load or pre-existing flake), the
/// end-of-function cleanup never ran and the auto-forked daemon was
/// reparented to init forever, accumulating across test runs and
/// breaking subsequent daemon-spawning tests with stale-socket /
/// IPC-timeout failures.
///
/// Usage:
/// ```ignore
/// let _stop = DaemonAutoStop::for_target(target);
/// loopr().args(["-C", target.to_str().unwrap(), "plan", "create", "x"]).assert().success();
/// // _stop drops at end of scope, SIGTERMs the daemon either way.
/// ```
#[must_use = "DaemonAutoStop leaks daemons unless bound to a variable for its scope"]
pub struct DaemonAutoStop {
    target: PathBuf,
}

impl DaemonAutoStop {
    pub fn for_target(target: &Path) -> Self {
        Self {
            target: target.to_path_buf(),
        }
    }
}

impl Drop for DaemonAutoStop {
    fn drop(&mut self) {
        stop_daemon_for(&self.target);
    }
}

/// SIGTERM the daemon whose PID is recorded at `<target>/.loopr/daemon.pid`,
/// waiting up to 5s for it to exit before SIGKILLing. Idempotent: no PID
/// file or already-dead daemon both return cleanly. Public so individual
/// test files can call it directly when an explicit shutdown ordering is
/// required; prefer `DaemonAutoStop` for the panic-safe path.
pub fn stop_daemon_for(target: &Path) {
    let pid_file = target.join(".loopr").join("daemon.pid");
    let pid: u32 = match fs::read_to_string(&pid_file) {
        Ok(s) => match s.trim().parse() {
            Ok(p) => p,
            Err(_) => return,
        },
        Err(_) => return,
    };
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

/// Initialize a git repo at `path` with a single empty commit so HEAD
/// exists. Stage 8 wiring's `handle_plan_create` calls
/// `ensure_integration_branch`, which requires a valid HEAD to branch
/// from; a bare tempdir has neither. `commit.gpgsign` and `tag.gpgsign`
/// are explicitly disabled because the test host may inherit a
/// user-level git config with signing required.
pub fn init_git_repo(path: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    run(&["config", "commit.gpgsign", "false"]);
    run(&["config", "tag.gpgsign", "false"]);
    run(&["commit", "--allow-empty", "-q", "-m", "initial"]);
}
