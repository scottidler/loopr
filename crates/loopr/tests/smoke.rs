//! Smoke tests for the Stage 1 + Stage 2 exit criteria. Exercises the
//! compiled binary end-to-end via `assert_cmd`; each invocation runs in its
//! own subprocess so the global tracing subscriber is fresh every time.

#![allow(clippy::unwrap_used)]

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn loopr() -> Command {
    Command::cargo_bin("loopr").unwrap()
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
fn help_lists_all_stage_subcommands() {
    let expected_subcommands = [
        "init",
        "plan",
        "decompose",
        "execute",
        "integrate",
        "daemon",
        "score",
        "logs",
        "list",
    ];
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
fn plan_on_tempdir_returns_stage_unimplemented() {
    // Tempdir has no source-guard and no .loopr/.taskstore markers. The
    // resolver falls through, the guard passes, telemetry initializes (writes
    // log files into the tempdir), and the stub errors with Stage 5.
    let td = TempDir::new().unwrap();
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 5"));
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
fn daemon_start_returns_stage_4() {
    let td = TempDir::new().unwrap();
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "daemon", "start"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 4"));
}

#[test]
fn score_returns_stage_9() {
    let td = TempDir::new().unwrap();
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "score", "--dir", "/tmp/run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 9"));
}

// ---------- Stage 2 smoke tests ----------

fn run_plan(target: &std::path::Path) {
    loopr()
        .args(["-C", target.to_str().unwrap(), "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 5"));
}

#[test]
fn plan_writes_events_and_pretty_logs() {
    let td = TempDir::new().unwrap();
    run_plan(td.path());

    let runs_dir = td.path().join(".loopr").join("runs");
    assert!(runs_dir.is_dir(), "runs dir exists: {}", runs_dir.display());
    let mut run_dirs: Vec<_> = fs::read_dir(&runs_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert_eq!(run_dirs.len(), 1, "exactly one run created");
    let run_dir = run_dirs.pop().unwrap();
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
}

#[test]
fn events_log_is_valid_json_with_expected_span() {
    let td = TempDir::new().unwrap();
    run_plan(td.path());

    let runs_dir = td.path().join(".loopr").join("runs");
    let run_dir = fs::read_dir(&runs_dir).unwrap().next().unwrap().unwrap().path();
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
    assert_eq!(lines.len(), 2, "two runs listed: {stdout}");
    let first_id = lines[0].split_whitespace().next().unwrap();
    let second_id = lines[1].split_whitespace().next().unwrap();
    assert!(first_id > second_id, "newest first: {first_id} > {second_id}");
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
    let run_dir = fs::read_dir(td.path().join(".loopr").join("runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let pretty = fs::read_to_string(run_dir.join("loopr.log")).unwrap();
    assert!(
        !pretty.contains("at debug level"),
        "debug event suppressed at info default: {pretty}"
    );
}

#[test]
fn log_level_debug_emits_debug_events() {
    let td = TempDir::new().unwrap();
    loopr()
        .args(["-C", td.path().to_str().unwrap(), "--log-level", "debug", "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 5"));
    let run_dir = fs::read_dir(td.path().join(".loopr").join("runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let pretty = fs::read_to_string(run_dir.join("loopr.log")).unwrap();
    assert!(
        pretty.contains("at debug level"),
        "debug event present with --log-level debug: {pretty}"
    );
}

#[test]
fn log_level_via_env_var() {
    let td = TempDir::new().unwrap();
    loopr()
        .env("LOOPR_LOG_LEVEL", "debug")
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 5"));
    let run_dir = fs::read_dir(td.path().join(".loopr").join("runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let pretty = fs::read_to_string(run_dir.join("loopr.log")).unwrap();
    assert!(
        pretty.contains("at debug level"),
        "env var works same as --log-level: {pretty}"
    );
}

#[test]
fn console_layer_gated_on_tty() {
    // assert_cmd pipes stderr (not a TTY), so the console layer should be
    // suppressed. The invocation-span trace should NOT appear on stderr; the
    // only stderr content is eyre's StageUnimplemented message.
    let td = TempDir::new().unwrap();
    let assertion = loopr()
        .args(["-C", td.path().to_str().unwrap(), "plan", "x"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assertion.get_output().stderr).to_string();
    assert!(
        !stderr.contains("loopr.invocation"),
        "console layer suppressed when stderr is piped: {stderr}"
    );
}
