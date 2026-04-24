#![allow(clippy::unwrap_used)]

use clap::Parser;

use super::*;

fn parse_cmd(args: &[&str]) -> Command {
    Cli::parse_from(args).command
}

fn stub_run_id() -> telemetry::RunId {
    telemetry::RunId::parse("20260419-000000").unwrap()
}

#[test]
fn run_init_returns_stage_5() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        None,
        parse_cmd(&["loopr", "init"]),
    )
    .unwrap_err();
    match err {
        LooprError::StageUnimplemented { stage, subcommand } => {
            assert_eq!(stage, 5);
            assert_eq!(subcommand, "init");
        }
        other => panic!("expected StageUnimplemented, got {other:?}"),
    }
}

// `run_plan_returns_stage_5` used to assert that `dispatch` returned the
// stub `StageUnimplemented { stage: 5 }` directly. Phase 5 replaces that
// stub with a real client round-trip: plan() now tries to connect to a
// daemon at `<target>/.loopr/socket`. The daemon is live in an E2E test
// (see `tests/smoke.rs::plan_on_tempdir_returns_stage_unimplemented`),
// not in a pure unit test. The unit-level coverage moves to the per-
// function coverage of `daemon_stop` / `daemon_status` below.

// `run_daemon_start_returns_stage_4` was removed at Stage 4 Phase 3:
// `daemon start` (background) is now handled by `lib::run`'s pre-
// telemetry fork hoist and never reaches `dispatch`. The background-fork
// behavior is smoke-tested end-to-end in
// `tests/smoke.rs::daemon_start_forks_daemon_and_writes_sentinels`.

#[test]
fn run_daemon_stop_on_empty_target_prints_no_daemon() {
    // Phase 5: `daemon stop` with no daemon running returns Ok(()) and
    // prints "no daemon running" (smoke-tested separately for stdout).
    let td = tempfile::TempDir::new().unwrap();
    dispatch(td.path(), &stub_run_id(), None, parse_cmd(&["loopr", "daemon", "stop"])).unwrap();
}

#[test]
fn run_daemon_status_on_empty_target_prints_no_daemon() {
    // Phase 5: `daemon status` with no pid file returns Ok(()) and
    // prints "no daemon running". Does NOT attempt to connect (would
    // hang on connect_or_wait without a daemon).
    let td = tempfile::TempDir::new().unwrap();
    dispatch(
        td.path(),
        &stub_run_id(),
        None,
        parse_cmd(&["loopr", "daemon", "status"]),
    )
    .unwrap();
}

#[test]
fn run_logs_tail_on_empty_target_errors_no_runs_found() {
    let td = tempfile::TempDir::new().unwrap();
    let err = dispatch(td.path(), &stub_run_id(), None, parse_cmd(&["loopr", "logs", "tail"])).unwrap_err();
    match err {
        LooprError::LogsQuery(msg) => assert!(msg.contains("no runs found"), "msg: {msg}"),
        other => panic!("expected LogsQuery(no runs found), got {other:?}"),
    }
}

#[test]
fn run_logs_runs_on_empty_target_succeeds_with_no_output() {
    let td = tempfile::TempDir::new().unwrap();
    // list_runs on a target with no .loopr/runs returns empty vec -> Ok(())
    dispatch(td.path(), &stub_run_id(), None, parse_cmd(&["loopr", "logs", "runs"])).unwrap();
}

// ---------- resolve_log_directive ----------
//
// Env-variable precedence (flag > env > default) is covered by the smoke
// tests that run the compiled binary in a subprocess - that's the only way
// to isolate env state when cargo runs unit tests in parallel. The unit
// tests here cover only directive assembly, which is pure.

#[test]
fn resolve_log_directive_flag_passes_through() {
    let d = resolve_log_directive(Some("debug"));
    assert_eq!(d, "debug");
}

#[test]
fn resolve_log_directive_per_target_passes_through() {
    let d = resolve_log_directive(Some("loopr=debug,tools=error"));
    assert_eq!(d, "loopr=debug,tools=error");
}

#[test]
fn resolve_log_directive_off_passes_through() {
    let d = resolve_log_directive(Some("off"));
    assert_eq!(d, "off");
}
