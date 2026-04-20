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

#[test]
fn run_plan_returns_stage_5() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        parse_cmd(&["loopr", "plan", "goal"]),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 5,
            subcommand: "plan"
        }
    ));
}

#[test]
fn run_decompose_returns_stage_6() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        parse_cmd(&["loopr", "decompose", "plan-1"]),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 6,
            subcommand: "decompose"
        }
    ));
}

#[test]
fn run_execute_returns_stage_7() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        parse_cmd(&["loopr", "execute"]),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 7,
            subcommand: "execute"
        }
    ));
}

#[test]
fn run_integrate_returns_stage_8() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        parse_cmd(&["loopr", "integrate"]),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 8,
            subcommand: "integrate"
        }
    ));
}

#[test]
fn run_daemon_start_returns_stage_4() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        parse_cmd(&["loopr", "daemon", "start"]),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 4,
            subcommand: "daemon-start"
        }
    ));
}

#[test]
fn run_daemon_stop_returns_stage_4() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        parse_cmd(&["loopr", "daemon", "stop"]),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 4,
            subcommand: "daemon-stop"
        }
    ));
}

#[test]
fn run_daemon_status_returns_stage_4() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        parse_cmd(&["loopr", "daemon", "status"]),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 4,
            subcommand: "daemon-status"
        }
    ));
}

#[test]
fn run_score_returns_stage_9() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        parse_cmd(&["loopr", "score", "--dir", "/tmp/run"]),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 9,
            subcommand: "score"
        }
    ));
}

#[test]
fn run_logs_tail_on_empty_target_errors_no_runs_found() {
    let td = tempfile::TempDir::new().unwrap();
    let err = dispatch(td.path(), &stub_run_id(), parse_cmd(&["loopr", "logs", "tail"])).unwrap_err();
    match err {
        LooprError::LogsQuery(msg) => assert!(msg.contains("no runs found"), "msg: {msg}"),
        other => panic!("expected LogsQuery(no runs found), got {other:?}"),
    }
}

#[test]
fn run_logs_runs_on_empty_target_succeeds_with_no_output() {
    let td = tempfile::TempDir::new().unwrap();
    // list_runs on a target with no .loopr/runs returns empty vec -> Ok(())
    dispatch(td.path(), &stub_run_id(), parse_cmd(&["loopr", "logs", "runs"])).unwrap();
}

#[test]
fn run_list_returns_stage_5() {
    let err = dispatch(
        std::path::Path::new("/tmp"),
        &stub_run_id(),
        parse_cmd(&["loopr", "list", "plans"]),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 5,
            subcommand: "list"
        }
    ));
}

// ---------- resolve_log_filter ----------
//
// Env-variable precedence (flag > env > default) is covered by the smoke
// tests that run the compiled binary in a subprocess - that's the only way
// to isolate env state when cargo runs unit tests in parallel. The unit
// tests here cover only directive parsing, which is pure.

#[test]
fn resolve_log_filter_bare_level_parses() {
    let f = resolve_log_filter(Some("debug")).unwrap();
    assert!(f.to_string().contains("debug"), "directive: {}", f);
}

#[test]
fn resolve_log_filter_per_target_directive_parses() {
    let f = resolve_log_filter(Some("loopr=debug,tools=error")).unwrap();
    let s = f.to_string();
    assert!(s.contains("loopr"), "contains loopr: {s}");
    assert!(s.contains("tools"), "contains tools: {s}");
}

#[test]
fn resolve_log_filter_off_parses() {
    // EnvFilter is permissive enough that constructing a genuinely invalid
    // directive is awkward; cover the edge case that "off" is accepted.
    let f = resolve_log_filter(Some("off")).unwrap();
    assert_eq!(f.to_string(), "off");
}
