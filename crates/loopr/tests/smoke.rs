//! Smoke tests for the Stage 1 + Stage 2 exit criteria. Exercises the
//! compiled binary end-to-end via `assert_cmd`; each invocation runs in its
//! own subprocess so the global tracing subscriber is fresh every time.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

mod common;
use common::init_git_repo;

fn loopr() -> Command {
    Command::cargo_bin("loopr").unwrap()
}

/// Stage 4 Phase 3+ hook: client commands that need a daemon (plan,
/// daemon status, and later list/show) auto-fork one on first use.
/// That leaves a background process alive past the end of the test,
/// which leaks state into the next test and prevents the `TempDir` from
/// cleaning up (the daemon holds the log directory open).
///
/// Every smoke test that triggers an auto-fork must `defer` this helper
/// to SIGTERM the daemon and wait for it to exit. Reads
/// `.loopr/daemon.pid` directly (Phase 3 has no `daemon stop` subcommand
/// yet; that lands in Phase 5).
fn stop_daemon(target: &std::path::Path) {
    let pid_file = target.join(".loopr").join("daemon.pid");
    let pid: u32 = match fs::read_to_string(&pid_file) {
        Ok(s) => match s.trim().parse() {
            Ok(p) => p,
            Err(_) => return,
        },
        Err(_) => return,
    };
    // SAFETY: kill with SIGTERM on a known PID. Worst case the process is
    // already gone and we get ESRCH, which we ignore.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        // SAFETY: kill(pid, 0) probes liveness.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if alive != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Escalate.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

/// Return the run-dir that is NOT the daemon's. The daemon writes its
/// own run-id to `.loopr/daemon.run-id`; every other dir under
/// `.loopr/runs/` belongs to a client invocation.
fn client_run_dirs(target: &std::path::Path) -> Vec<std::path::PathBuf> {
    let daemon_run_id = fs::read_to_string(target.join(".loopr").join("daemon.run-id"))
        .ok()
        .map(|s| s.trim().to_string());
    let runs_dir = target.join(".loopr").join("runs");
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&runs_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if daemon_run_id.as_deref() == Some(name.as_str()) {
                continue;
            }
            out.push(entry.path());
        }
    }
    out
}

#[test]
fn version_prints_something_sensible() {
    loopr()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"^loopr v?\d+\.\d+\.\d+").unwrap());
}

#[test]
fn help_lists_surviving_subcommands() {
    let expected_subcommands = ["init", "plan", "daemon", "logs"];
    let mut cmd = loopr();
    let output = cmd.arg("--help").assert().success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).to_string();
    for sc in expected_subcommands {
        assert!(
            stdout.contains(sc),
            "expected `loopr --help` to mention `{sc}`; full help output:\n{stdout}"
        );
    }
}

#[test]
fn plan_on_tempdir_creates_and_prints_plan() {
    // Stage 5: tempdir with no pre-existing .loopr marker. The resolver
    // falls through, the guard passes, the daemon auto-forks, the store
    // opens under .loopr/taskstore/, and plan.create returns the record.
    // Stage 8 wiring adds a git-init requirement because handle_plan_create
    // now creates an integration branch before persisting the Plan.
    let td = TempDir::new().unwrap();
    init_git_repo(td.path());
    let output = loopr()
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).to_string();
    assert!(
        stdout.contains("plan:"),
        "stdout prints the created plan id line: {stdout}"
    );
    assert!(stdout.contains("goal:"), "stdout prints the goal line: {stdout}");
    assert!(stdout.contains("  x"), "stdout echoes the goal text: {stdout}");

    stop_daemon(td.path());
}

#[test]
fn source_guard_blocks_target_with_sentinel() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join(".loopr-source-guard"), "").unwrap();
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("source tree"));
}

#[test]
fn source_guard_trips_from_within_loopr_v5_checkout() {
    // Run the binary with CWD inside the loopr-v5 crate (no -C). Target
    // resolution walks to the git root (loopr-v5/), and the source-guard
    // walks ancestors to find the .loopr-source-guard sentinel committed
    // at the repo root. This is the live-fire check that the sentinel
    // actually blocks loopr from operating on its own source tree.
    loopr()
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(".loopr-source-guard"));
}

#[test]
fn target_invalid_when_path_does_not_exist() {
    loopr()
        .args(["-C", "/does/not/exist/42", "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));
}

#[test]
fn target_is_file_hints_at_parent() {
    let td = TempDir::new().unwrap();
    let file = td.path().join("a-file");
    fs::write(&file, "").unwrap();
    loopr()
        .args(["-C", file.to_str().unwrap(), "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is a file"))
        .stderr(predicate::str::contains("try -C"));
}

#[test]
fn daemon_start_forks_daemon_and_writes_sentinels() {
    // Stage 4 Phase 3: `daemon start` forks a background daemon that
    // writes pid / version / run-id sentinels and binds the socket, then
    // awaits shutdown. The client-side caller returns immediately.
    let td = TempDir::new().unwrap();
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "daemon", "start"])
        .assert()
        .success()
        .stdout(predicate::str::contains("daemon started"));
    let loopr_dir = td.path().join(".loopr");
    assert!(loopr_dir.join("daemon.pid").is_file(), "pid file present");
    assert!(loopr_dir.join("daemon.version").is_file(), "version file present");
    assert!(loopr_dir.join("daemon.run-id").is_file(), "run-id file present");
    assert!(loopr_dir.join("socket").exists(), "socket bound");

    stop_daemon(td.path());
    // After SIGTERM the daemon cleans its own sentinels. Give the kernel
    // a beat before dropping the TempDir.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && loopr_dir.join("daemon.pid").exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------- Stage 2 smoke tests ----------

fn run_plan(target: &std::path::Path) {
    // Stage 5: `loopr plan "x"` now succeeds — it auto-forks a daemon,
    // handshakes, persists the Plan, and prints the record. These log-
    // harness tests care about run-dir layout and subscriber behavior,
    // not the subcommand's exit code; we just need a successful client
    // invocation to exercise the telemetry pipeline.
    // Stage 8 wiring requires a git-initialized target for plan.create.
    init_git_repo(target);
    loopr()
        .args(["-C", target.to_str().unwrap(), "plan", "x"])
        .assert()
        .success();
}

#[test]
fn plan_writes_events_and_pretty_logs() {
    let td = TempDir::new().unwrap();
    run_plan(td.path());

    let runs_dir = td.path().join(".loopr").join("runs");
    assert!(runs_dir.is_dir(), "runs dir exists: {}", runs_dir.display());
    // Stage 4 Phase 3: `plan` auto-forks a daemon before the client's own
    // telemetry init, so the runs dir contains TWO run-id dirs (the
    // daemon's and the client's). We want the CLIENT's pretty log to
    // carry the invocation span.
    let client_dirs = client_run_dirs(td.path());
    assert_eq!(client_dirs.len(), 1, "exactly one client run: {client_dirs:?}");
    let run_dir = &client_dirs[0];
    let events = run_dir.join("events.log");
    let pretty = run_dir.join("loopr.log");
    assert!(events.is_file(), "events.log exists");
    assert!(pretty.is_file(), "loopr.log exists");
    let events_body = fs::read_to_string(&events).unwrap();
    let pretty_body = fs::read_to_string(&pretty).unwrap();
    assert!(!events_body.is_empty(), "events.log non-empty");
    assert!(!pretty_body.is_empty(), "loopr.log non-empty");
    assert!(
        pretty_body.contains("loopr.invocation"),
        "pretty log contains invocation span: {pretty_body}"
    );

    stop_daemon(td.path());
}

#[test]
fn events_log_is_valid_json_with_expected_span() {
    let td = TempDir::new().unwrap();
    run_plan(td.path());

    // Stage 4 Phase 3: `plan` auto-forks a daemon; pick the client's run
    // dir (not the daemon's) so we exercise the invocation span carried
    // by the CLIENT subscriber.
    let client_dirs = client_run_dirs(td.path());
    assert_eq!(client_dirs.len(), 1);
    let run_dir = &client_dirs[0];
    let run_id = run_dir.file_name().unwrap().to_str().unwrap().to_string();
    let events = run_dir.join("events.log");
    let body = fs::read_to_string(&events).unwrap();

    let mut saw_invocation = false;
    for line in body.lines() {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|_| panic!("events.log line not JSON: {line}"));
        assert!(v.get("timestamp").is_some(), "line has timestamp: {line}");
        assert!(v.get("level").is_some(), "line has level");
        if let Some(spans) = v.get("spans").and_then(|s| s.as_array()) {
            for s in spans {
                if s.get("name") == Some(&serde_json::Value::String("loopr.invocation".into())) {
                    assert_eq!(
                        s.get("run_id").and_then(|r| r.as_str()),
                        Some(run_id.as_str()),
                        "invocation span carries run_id = dir name"
                    );
                    saw_invocation = true;
                }
            }
        }
    }
    assert!(saw_invocation, "at least one event inside loopr.invocation");

    stop_daemon(td.path());
}

#[test]
fn logs_tail_reads_pretty_from_latest_run() {
    let td = TempDir::new().unwrap();
    run_plan(td.path());
    let output = loopr()
        .args(["-C", td.path().to_str().unwrap(), "logs", "tail", "--lines", "10"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).to_string();
    assert!(!stdout.is_empty(), "tail stdout non-empty: {stdout}");
    assert!(
        stdout.contains("loopr.invocation"),
        "tail stdout contains invocation span: {stdout}"
    );

    stop_daemon(td.path());
}

#[test]
fn logs_runs_lists_newest_first() {
    let td = TempDir::new().unwrap();
    run_plan(td.path());
    // Sleep briefly to force a distinct second-granularity run id; without
    // this, same-second collisions produce -2 suffixes which still sort
    // newest-first but we want to show the sort isn't trivial.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    run_plan(td.path());

    let output = loopr()
        .args(["-C", td.path().to_str().unwrap(), "logs", "runs"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).to_string();
    let lines: Vec<&str> = stdout.lines().collect();
    // Stage 4 Phase 3: the first `plan` auto-forks a daemon (so the daemon's
    // run-id becomes a third listed dir). The second `plan` sees the live
    // daemon via `ensure_daemon_if_needed` and does not re-fork. Expect
    // three entries total: two client-side run-ids plus the daemon's.
    assert_eq!(lines.len(), 3, "three runs listed: {stdout}");
    // Ordering is still newest-first by run-id string sort.
    for window in lines.windows(2) {
        let a = window[0].split_whitespace().next().unwrap();
        let b = window[1].split_whitespace().next().unwrap();
        assert!(a >= b, "newest first: {a} >= {b}");
    }

    stop_daemon(td.path());
}

#[test]
fn logs_tail_no_runs_errors_cleanly() {
    let td = TempDir::new().unwrap();
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "logs", "tail"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no runs found"));
}

#[test]
fn log_level_gate_suppresses_debug_at_info_default() {
    let td = TempDir::new().unwrap();
    run_plan(td.path());
    // Stage 4 Phase 3: use the client run dir, not whichever dir happens
    // to be first from read_dir. The daemon writes to its own run-id,
    // which on a fast fork may also contain events.
    let client_dirs = client_run_dirs(td.path());
    let run_dir = &client_dirs[0];
    let pretty = fs::read_to_string(run_dir.join("loopr.log")).unwrap();
    assert!(
        !pretty.contains("at debug level"),
        "debug event suppressed at info default: {pretty}"
    );

    stop_daemon(td.path());
}

#[test]
fn log_level_debug_emits_debug_events() {
    let td = TempDir::new().unwrap();
    init_git_repo(td.path());
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "--log-level", "debug", "plan", "x"])
        .assert()
        .success();
    let client_dirs = client_run_dirs(td.path());
    let run_dir = &client_dirs[0];
    let pretty = fs::read_to_string(run_dir.join("loopr.log")).unwrap();
    assert!(
        pretty.contains("at debug level"),
        "debug event present with --log-level debug: {pretty}"
    );

    stop_daemon(td.path());
}

#[test]
fn log_level_via_env_var() {
    let td = TempDir::new().unwrap();
    init_git_repo(td.path());
    loopr()
        .env("LOOPR_LOG_LEVEL", "debug")
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .success();
    let client_dirs = client_run_dirs(td.path());
    let run_dir = &client_dirs[0];
    let pretty = fs::read_to_string(run_dir.join("loopr.log")).unwrap();
    assert!(
        pretty.contains("at debug level"),
        "env var works same as --log-level: {pretty}"
    );

    stop_daemon(td.path());
}

#[test]
fn console_layer_gated_on_tty() {
    // assert_cmd pipes stderr (not a TTY), so the console layer should be
    // suppressed. The invocation-span trace should NOT appear on stderr.
    // Stage 5: `plan` now succeeds and prints the plan to stdout; stderr
    // should be empty of the invocation span regardless.
    let td = TempDir::new().unwrap();
    init_git_repo(td.path());
    let assertion = loopr()
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        !stderr.contains("loopr.invocation"),
        "console layer suppressed when stderr is piped: {stderr}"
    );

    stop_daemon(td.path());
}
