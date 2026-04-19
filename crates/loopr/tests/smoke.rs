//! Smoke tests for the Stage 1 exit criteria. Exercises the compiled binary
//! end-to-end via `assert_cmd`; cheap to run, catches regressions that unit
//! tests miss (argv wiring, eyre termination, actual `--version` output).

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
fn plan_on_tmp_returns_stage_unimplemented() {
    // /tmp has no source-guard and no .loopr/.taskstore markers. The resolver
    // should fall through, the guard should pass, and the stub should error
    // with StageUnimplemented { stage: 5, ... }. eyre flattens to exit 1.
    loopr()
        .args(["-C", "/tmp", "plan", "x"])
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
    loopr()
        .args(["-C", "/tmp", "daemon", "start"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 4"));
}

#[test]
fn score_returns_stage_9() {
    loopr()
        .args(["-C", "/tmp", "score", "--dir", "/tmp/run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Stage 9"));
}
