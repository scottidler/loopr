#![allow(clippy::unwrap_used)]

use clap::Parser;

use super::*;

fn parse_cmd(args: &[&str]) -> Command {
    Cli::parse_from(args).command
}

#[test]
fn run_init_returns_stage_5() {
    let err = dispatch(parse_cmd(&["loopr", "init"])).unwrap_err();
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
    let err = dispatch(parse_cmd(&["loopr", "plan", "goal"])).unwrap_err();
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
    let err = dispatch(parse_cmd(&["loopr", "decompose", "plan-1"])).unwrap_err();
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
    let err = dispatch(parse_cmd(&["loopr", "execute"])).unwrap_err();
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
    let err = dispatch(parse_cmd(&["loopr", "integrate"])).unwrap_err();
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
    let err = dispatch(parse_cmd(&["loopr", "daemon", "start"])).unwrap_err();
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
    let err = dispatch(parse_cmd(&["loopr", "daemon", "stop"])).unwrap_err();
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
    let err = dispatch(parse_cmd(&["loopr", "daemon", "status"])).unwrap_err();
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
    let err = dispatch(parse_cmd(&["loopr", "score", "--dir", "/tmp/run"])).unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 9,
            subcommand: "score"
        }
    ));
}

#[test]
fn run_logs_tail_returns_stage_2() {
    let err = dispatch(parse_cmd(&["loopr", "logs", "tail"])).unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 2,
            subcommand: "logs-tail"
        }
    ));
}

#[test]
fn run_logs_runs_returns_stage_2() {
    let err = dispatch(parse_cmd(&["loopr", "logs", "runs"])).unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 2,
            subcommand: "logs-runs"
        }
    ));
}

#[test]
fn run_list_returns_stage_5() {
    let err = dispatch(parse_cmd(&["loopr", "list", "plans"])).unwrap_err();
    assert!(matches!(
        err,
        LooprError::StageUnimplemented {
            stage: 5,
            subcommand: "list"
        }
    ));
}
