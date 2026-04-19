#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use clap::{CommandFactory, Parser};

use super::*;

#[test]
fn test_cli_verify() {
    Cli::command().debug_assert();
}

#[test]
fn test_cli_parses_init() {
    let cli = Cli::parse_from(["loopr", "init"]);
    assert!(matches!(cli.command, Command::Init));
}

#[test]
fn test_cli_parses_plan() {
    let cli = Cli::parse_from(["loopr", "plan", "add --version flag"]);
    match cli.command {
        Command::Plan { goal } => assert_eq!(goal, "add --version flag"),
        _ => panic!("expected Plan"),
    }
}

#[test]
fn test_cli_parses_decompose() {
    let cli = Cli::parse_from(["loopr", "decompose", "plan-123"]);
    match cli.command {
        Command::Decompose { plan_id } => assert_eq!(plan_id, "plan-123"),
        _ => panic!("expected Decompose"),
    }
}

#[test]
fn test_cli_parses_execute_bare() {
    let cli = Cli::parse_from(["loopr", "execute"]);
    match cli.command {
        Command::Execute { work_id } => assert!(work_id.is_none()),
        _ => panic!("expected Execute"),
    }
}

#[test]
fn test_cli_parses_execute_with_work_id() {
    let cli = Cli::parse_from(["loopr", "execute", "--work-id", "wi-1"]);
    match cli.command {
        Command::Execute { work_id } => assert_eq!(work_id.as_deref(), Some("wi-1")),
        _ => panic!("expected Execute with work_id"),
    }
}

#[test]
fn test_cli_parses_integrate() {
    let cli = Cli::parse_from(["loopr", "integrate"]);
    assert!(matches!(cli.command, Command::Integrate));
}

#[test]
fn test_cli_parses_daemon_start() {
    let cli = Cli::parse_from(["loopr", "daemon", "start"]);
    match cli.command {
        Command::Daemon {
            cmd: DaemonCmd::Start { foreground },
        } => {
            assert!(!foreground);
        }
        _ => panic!("expected Daemon Start"),
    }
}

#[test]
fn test_cli_parses_daemon_start_foreground() {
    let cli = Cli::parse_from(["loopr", "daemon", "start", "--foreground"]);
    match cli.command {
        Command::Daemon {
            cmd: DaemonCmd::Start { foreground },
        } => {
            assert!(foreground);
        }
        _ => panic!("expected Daemon Start --foreground"),
    }
}

#[test]
fn test_cli_parses_daemon_stop() {
    let cli = Cli::parse_from(["loopr", "daemon", "stop"]);
    assert!(matches!(cli.command, Command::Daemon { cmd: DaemonCmd::Stop }));
}

#[test]
fn test_cli_parses_daemon_status() {
    let cli = Cli::parse_from(["loopr", "daemon", "status"]);
    assert!(matches!(cli.command, Command::Daemon { cmd: DaemonCmd::Status }));
}

#[test]
fn test_cli_parses_score() {
    let cli = Cli::parse_from(["loopr", "score", "--dir", "/tmp/run1"]);
    match cli.command {
        Command::Score { dir, duration_secs } => {
            assert_eq!(dir, PathBuf::from("/tmp/run1"));
            assert_eq!(duration_secs, 0);
        }
        _ => panic!("expected Score"),
    }
}

#[test]
fn test_cli_parses_score_with_duration() {
    let cli = Cli::parse_from(["loopr", "score", "-d", "/tmp/run1", "--duration-secs", "120"]);
    match cli.command {
        Command::Score { dir, duration_secs } => {
            assert_eq!(dir, PathBuf::from("/tmp/run1"));
            assert_eq!(duration_secs, 120);
        }
        _ => panic!("expected Score with duration"),
    }
}

#[test]
fn test_cli_parses_logs_tail() {
    let cli = Cli::parse_from(["loopr", "logs", "tail"]);
    match cli.command {
        Command::Logs {
            cmd: LogsCmd::Tail { lines },
        } => {
            assert_eq!(lines, 100);
        }
        _ => panic!("expected Logs Tail"),
    }
}

#[test]
fn test_cli_parses_logs_tail_with_lines() {
    let cli = Cli::parse_from(["loopr", "logs", "tail", "--lines", "50"]);
    match cli.command {
        Command::Logs {
            cmd: LogsCmd::Tail { lines },
        } => {
            assert_eq!(lines, 50);
        }
        _ => panic!("expected Logs Tail --lines 50"),
    }
}

#[test]
fn test_cli_parses_logs_runs() {
    let cli = Cli::parse_from(["loopr", "logs", "runs"]);
    assert!(matches!(cli.command, Command::Logs { cmd: LogsCmd::Runs }));
}

#[test]
fn test_cli_parses_list() {
    let cli = Cli::parse_from(["loopr", "list", "plans"]);
    match cli.command {
        Command::List { kind } => assert_eq!(kind, "plans"),
        _ => panic!("expected List"),
    }
}

#[test]
fn test_cli_parses_chdir_global() {
    let cli = Cli::parse_from(["loopr", "-C", "/tmp/target", "plan", "x"]);
    assert_eq!(cli.chdir, Some(PathBuf::from("/tmp/target")));
    match cli.command {
        Command::Plan { goal } => assert_eq!(goal, "x"),
        _ => panic!("expected Plan"),
    }
}

#[test]
fn test_cli_parses_chdir_long() {
    let cli = Cli::parse_from(["loopr", "--chdir", "/tmp/target", "init"]);
    assert_eq!(cli.chdir, Some(PathBuf::from("/tmp/target")));
}
