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

#[test]
fn test_cli_parses_log_level_short() {
    let cli = Cli::parse_from(["loopr", "-l", "debug", "init"]);
    assert_eq!(cli.log_level.as_deref(), Some("debug"));
}

#[test]
fn test_cli_parses_log_level_long() {
    let cli = Cli::parse_from(["loopr", "--log-level", "loopr=debug,tools=error", "init"]);
    assert_eq!(cli.log_level.as_deref(), Some("loopr=debug,tools=error"));
}

#[test]
fn test_cli_log_level_defaults_to_none() {
    let cli = Cli::parse_from(["loopr", "init"]);
    assert!(cli.log_level.is_none());
}

#[test]
fn test_cli_log_level_is_global() {
    let cli = Cli::parse_from(["loopr", "plan", "-l", "debug", "goal"]);
    assert_eq!(cli.log_level.as_deref(), Some("debug"));
    match cli.command {
        Command::Plan { goal } => assert_eq!(goal, "goal"),
        _ => panic!("expected Plan"),
    }
}

#[test]
fn test_command_label_covers_every_variant() {
    let cases = [
        (&["loopr", "init"][..], "init"),
        (&["loopr", "plan", "x"][..], "plan"),
        (&["loopr", "daemon", "start"][..], "daemon-start"),
        (&["loopr", "daemon", "stop"][..], "daemon-stop"),
        (&["loopr", "daemon", "status"][..], "daemon-status"),
        (&["loopr", "logs", "tail"][..], "logs-tail"),
        (&["loopr", "logs", "runs"][..], "logs-runs"),
    ];
    for (argv, expected) in cases {
        let cli = Cli::parse_from(argv);
        assert_eq!(
            cli.command.label(),
            expected,
            "label for argv {argv:?} should be {expected}"
        );
    }
}
