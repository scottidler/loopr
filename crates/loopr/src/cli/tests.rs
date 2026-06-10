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
    assert!(matches!(cli.command, Some(Command::Init { force: false })));
}

#[test]
fn test_cli_parses_init_with_force() {
    let cli = Cli::parse_from(["loopr", "init", "--force"]);
    assert!(matches!(cli.command, Some(Command::Init { force: true })));
}

#[test]
fn test_cli_parses_plan() {
    let cli = Cli::parse_from(["loopr", "plan", "create", "add --version flag"]);
    match cli.command.unwrap() {
        Command::Plan {
            cmd: crate::cli::PlanCmd::Create { goal },
        } => assert_eq!(goal, "add --version flag"),
        _ => panic!("expected Plan::Create"),
    }
}

#[test]
fn test_cli_parses_plan_override() {
    let cli = Cli::parse_from(["loopr", "plan", "override", "pl-abc12", "--to", "active"]);
    match cli.command.unwrap() {
        Command::Plan {
            cmd: crate::cli::PlanCmd::Override { plan_id, to },
        } => {
            assert_eq!(plan_id, "pl-abc12");
            assert_eq!(to, PlanOverrideTo::Active);
        }
        _ => panic!("expected Plan::Override"),
    }
}

#[test]
fn test_cli_parses_plan_override_to_is_case_insensitive() {
    // `--to Stalled` (the cased form `show` displays) must parse, not error.
    let cli = Cli::parse_from(["loopr", "plan", "override", "pl-abc12", "--to", "Stalled"]);
    match cli.command.unwrap() {
        Command::Plan {
            cmd: crate::cli::PlanCmd::Override { to, .. },
        } => assert_eq!(to, PlanOverrideTo::Stalled),
        _ => panic!("expected Plan::Override"),
    }
}

#[test]
fn test_cli_parses_plan_override_to_rejects_unknown_status() {
    let res = Cli::try_parse_from(["loopr", "plan", "override", "pl-abc12", "--to", "bogus"]);
    assert!(res.is_err(), "unknown --to value must be rejected at parse time");
}

#[test]
fn test_cli_parses_output_uppercase_is_case_insensitive() {
    use crate::output::Format;
    let cli = Cli::parse_from(["loopr", "--output", "JSON", "plans"]);
    assert_eq!(cli.output, Some(Format::Json));
}

#[test]
fn test_cli_parses_plans() {
    let cli = Cli::parse_from(["loopr", "plans"]);
    assert!(matches!(cli.command, Some(Command::Plans)));
}

#[test]
fn test_cli_parses_works() {
    let cli = Cli::parse_from(["loopr", "works"]);
    assert!(matches!(cli.command, Some(Command::Works)));
}

#[test]
fn test_cli_parses_bundles() {
    let cli = Cli::parse_from(["loopr", "bundles"]);
    assert!(matches!(cli.command, Some(Command::Bundles)));
}

#[test]
fn test_cli_parses_ticks() {
    let cli = Cli::parse_from(["loopr", "ticks"]);
    assert!(matches!(cli.command, Some(Command::Ticks)));
}

#[test]
fn test_cli_parses_show() {
    let cli = Cli::parse_from(["loopr", "show", "pl-abcde"]);
    match cli.command.unwrap() {
        Command::Show { id } => assert_eq!(id, "pl-abcde"),
        _ => panic!("expected Show"),
    }
}

#[test]
fn test_cli_parses_output_json_global() {
    use crate::output::Format;
    let cli = Cli::parse_from(["loopr", "--output", "json", "plans"]);
    assert_eq!(cli.output, Some(Format::Json));
    assert!(matches!(cli.command, Some(Command::Plans)));
}

#[test]
fn test_cli_parses_output_yaml_short() {
    use crate::output::Format;
    let cli = Cli::parse_from(["loopr", "-o", "yaml", "plan", "create", "x"]);
    assert_eq!(cli.output, Some(Format::Yaml));
    match cli.command.unwrap() {
        Command::Plan {
            cmd: crate::cli::PlanCmd::Create { goal },
        } => assert_eq!(goal, "x"),
        _ => panic!("expected Plan::Create"),
    }
}

#[test]
fn test_cli_parses_daemon_start() {
    let cli = Cli::parse_from(["loopr", "daemon", "start"]);
    match cli.command.unwrap() {
        Command::Daemon {
            cmd: DaemonCmd::Start {
                foreground,
                accept_corruption,
            },
        } => {
            assert!(!foreground);
            assert!(!accept_corruption);
        }
        _ => panic!("expected Daemon Start"),
    }
}

#[test]
fn test_cli_parses_daemon_start_foreground() {
    let cli = Cli::parse_from(["loopr", "daemon", "start", "--foreground"]);
    match cli.command.unwrap() {
        Command::Daemon {
            cmd: DaemonCmd::Start {
                foreground,
                accept_corruption,
            },
        } => {
            assert!(foreground);
            assert!(!accept_corruption);
        }
        _ => panic!("expected Daemon Start --foreground"),
    }
}

#[test]
fn test_cli_parses_daemon_stop() {
    let cli = Cli::parse_from(["loopr", "daemon", "stop"]);
    assert!(matches!(cli.command, Some(Command::Daemon { cmd: DaemonCmd::Stop })));
}

#[test]
fn test_cli_parses_daemon_status() {
    let cli = Cli::parse_from(["loopr", "daemon", "status"]);
    assert!(matches!(cli.command, Some(Command::Daemon { cmd: DaemonCmd::Status })));
}

#[test]
fn test_cli_parses_logs_tail() {
    let cli = Cli::parse_from(["loopr", "logs", "tail"]);
    match cli.command.unwrap() {
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
    match cli.command.unwrap() {
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
    assert!(matches!(cli.command, Some(Command::Logs { cmd: LogsCmd::Runs })));
}

#[test]
fn test_cli_parses_tui_explicit() {
    let cli = Cli::parse_from(["loopr", "tui"]);
    assert!(matches!(cli.command, Some(Command::Tui)));
}

#[test]
fn test_cli_parses_bare_invocation_as_none() {
    let cli = Cli::parse_from(["loopr"]);
    assert!(cli.command.is_none(), "bare `loopr` must leave command unset");
}

#[test]
fn test_cli_parses_chdir_global() {
    let cli = Cli::parse_from(["loopr", "-C", "/tmp/target", "plan", "create", "x"]);
    assert_eq!(cli.chdir, Some(PathBuf::from("/tmp/target")));
    match cli.command.unwrap() {
        Command::Plan {
            cmd: crate::cli::PlanCmd::Create { goal },
        } => assert_eq!(goal, "x"),
        _ => panic!("expected Plan::Create"),
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
    let cli = Cli::parse_from(["loopr", "plan", "create", "-l", "debug", "goal"]);
    assert_eq!(cli.log_level.as_deref(), Some("debug"));
    match cli.command.unwrap() {
        Command::Plan {
            cmd: crate::cli::PlanCmd::Create { goal },
        } => assert_eq!(goal, "goal"),
        _ => panic!("expected Plan::Create"),
    }
}

#[test]
fn test_command_label_covers_every_variant() {
    let cases = [
        (&["loopr", "init"][..], "init"),
        (&["loopr", "plan", "create", "x"][..], "plan-create"),
        (
            &["loopr", "plan", "override", "pl-abc", "--to", "active"][..],
            "plan-override",
        ),
        (&["loopr", "plans"][..], "plans"),
        (&["loopr", "works"][..], "works"),
        (&["loopr", "bundles"][..], "bundles"),
        (&["loopr", "ticks"][..], "ticks"),
        (&["loopr", "show", "pl-abcde"][..], "show"),
        (&["loopr", "daemon", "start"][..], "daemon-start"),
        (&["loopr", "daemon", "stop"][..], "daemon-stop"),
        (&["loopr", "daemon", "status"][..], "daemon-status"),
        (&["loopr", "logs", "tail"][..], "logs-tail"),
        (&["loopr", "logs", "runs"][..], "logs-runs"),
        (&["loopr", "tui"][..], "tui"),
    ];
    for (argv, expected) in cases {
        let cli = Cli::parse_from(argv);
        assert_eq!(
            cli.command.unwrap().label(),
            expected,
            "label for argv {argv:?} should be {expected}"
        );
    }
}
