#![allow(clippy::unwrap_used)]

use clap::Parser;

use super::*;

fn parse_cmd(args: &[&str]) -> Command {
    Cli::parse_from(args)
        .command
        .expect("parse_cmd called with argv that has no subcommand")
}

fn stub_session_id() -> telemetry::SessionId {
    telemetry::SessionId::parse("20260419-000000").unwrap()
}

fn stub_process_id() -> telemetry::ProcessId {
    telemetry::ProcessId::parse("pc-test01").unwrap()
}

#[test]
fn run_init_seeds_prompts_into_target() {
    let td = tempfile::TempDir::new().unwrap();
    dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "init"]),
    )
    .unwrap();
    let seeded = td.path().join(".loopr/prompts/agents/implementer/system.pmt");
    assert!(seeded.exists(), "init should have seeded {seeded:?}");
}

// `run_plan_returns_stage_5` used to assert that `dispatch` returned a
// stub error directly. plan() now performs a real client round-trip: it
// tries to connect to a daemon at `<target>/.loopr/socket`. The daemon is
// live in an E2E test (see `tests/smoke.rs::plan_on_tempdir_returns_stage_unimplemented`),
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
    dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "daemon", "stop"]),
    )
    .unwrap();
}

#[test]
fn run_daemon_status_on_empty_target_prints_no_daemon() {
    // Phase 5: `daemon status` with no pid file returns Ok(()) and
    // prints "no daemon running". Does NOT attempt to connect (would
    // hang on connect_or_wait without a daemon).
    let td = tempfile::TempDir::new().unwrap();
    dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "daemon", "status"]),
    )
    .unwrap();
}

// ---------- Phase 16: read verbs report "no daemon" instead of forking ----------
//
// These call `dispatch` directly (never `daemon::ensure_daemon_if_needed`,
// which now excludes these commands per `lib::run`'s pre-fork arm). If any
// of these regressed back to auto-forking, the unconfigured `TempDir`
// target (no git repo, no `.loopr/config.yml`) would either hang in
// `connect_or_wait` waiting on a socket that never binds, or fail loudly
// on the daemon's own startup validation gate -- either way `dispatch`
// would NOT return `Ok(())` promptly, which is what every assertion below
// checks. No pid file existing afterward is the other half of the
// break-to-prove case: a daemon that got auto-forked leaves one behind.

#[test]
fn run_plans_on_empty_target_prints_no_daemon_and_does_not_fork() {
    let td = tempfile::TempDir::new().unwrap();
    dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "plans"]),
    )
    .unwrap();
    assert!(
        !td.path().join(".loopr").join("daemon.pid").exists(),
        "`loopr plans` on a quiet target must not auto-fork a daemon"
    );
}

#[test]
fn run_works_on_empty_target_prints_no_daemon_and_does_not_fork() {
    let td = tempfile::TempDir::new().unwrap();
    dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "works"]),
    )
    .unwrap();
    assert!(
        !td.path().join(".loopr").join("daemon.pid").exists(),
        "`loopr works` on a quiet target must not auto-fork a daemon"
    );
}

#[test]
fn run_bundles_on_empty_target_prints_no_daemon_and_does_not_fork() {
    let td = tempfile::TempDir::new().unwrap();
    dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "bundles"]),
    )
    .unwrap();
    assert!(
        !td.path().join(".loopr").join("daemon.pid").exists(),
        "`loopr bundles` on a quiet target must not auto-fork a daemon"
    );
}

#[test]
fn run_ticks_on_empty_target_prints_no_daemon_and_does_not_fork() {
    let td = tempfile::TempDir::new().unwrap();
    dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "ticks"]),
    )
    .unwrap();
    assert!(
        !td.path().join(".loopr").join("daemon.pid").exists(),
        "`loopr ticks` on a quiet target must not auto-fork a daemon"
    );
}

#[test]
fn run_show_on_empty_target_prints_no_daemon_and_does_not_fork() {
    let td = tempfile::TempDir::new().unwrap();
    dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "show", "pl-abc12"]),
    )
    .unwrap();
    assert!(
        !td.path().join(".loopr").join("daemon.pid").exists(),
        "`loopr show` on a quiet target must not auto-fork a daemon"
    );
}

#[test]
fn run_tui_returns_tui_not_installed() {
    // Explicit `loopr tui` reaches dispatch and should return
    // TuiNotInstalled until the TUI crate lands. Bare `loopr` (no
    // subcommand) is normalized to Command::Tui in lib::run before
    // dispatch, so exercising it via dispatch directly is equivalent.
    let td = tempfile::TempDir::new().unwrap();
    let err = dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "tui"]),
    )
    .unwrap_err();
    match err {
        LooprError::TuiNotInstalled => {}
        other => panic!("expected TuiNotInstalled, got {other:?}"),
    }
}

#[test]
fn run_logs_tail_on_empty_target_errors_no_runs_found() {
    let td = tempfile::TempDir::new().unwrap();
    let err = dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "logs", "tail"]),
    )
    .unwrap_err();
    match err {
        LooprError::LogsQuery(msg) => assert!(msg.contains("no runs found"), "msg: {msg}"),
        other => panic!("expected LogsQuery(no runs found), got {other:?}"),
    }
}

#[test]
fn run_logs_runs_on_empty_target_succeeds_with_no_output() {
    let td = tempfile::TempDir::new().unwrap();
    // list_runs on a target with no .loopr/runs returns empty vec -> Ok(())
    dispatch(
        td.path(),
        &stub_session_id(),
        &stub_process_id(),
        None,
        parse_cmd(&["loopr", "logs", "runs"]),
    )
    .unwrap();
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
