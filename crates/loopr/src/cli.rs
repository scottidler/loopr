use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "loopr",
    version = env!("GIT_DESCRIBE"),
    about = "Agent orchestrator; operates on a target repo via decompose/implement/integrate.",
    long_about = None,
)]
pub struct Cli {
    /// Change to <path> before doing anything (git-style).
    /// Falls back to $LOOPR_TARGET, then CWD.
    #[arg(short = 'C', long = "chdir", global = true, value_name = "PATH")]
    pub chdir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize loopr state (.loopr/, .taskstore/, git hooks) at the target. [Stage 5]
    Init,

    /// Submit a Plan goal to the daemon. [Stage 5]
    Plan {
        /// One-sentence goal to plan for.
        goal: String,
    },

    /// Decompose an existing Plan into a Work DAG. [Stage 6]
    Decompose {
        /// Plan ID to decompose.
        plan_id: String,
    },

    /// Run agents against ready Work items. [Stage 7]
    Execute {
        /// Restrict to a specific Work ID.
        #[arg(long)]
        work_id: Option<String>,
    },

    /// Integrate accepted Bundles into the integration branch. [Stage 8]
    Integrate,

    /// Daemon lifecycle (start, stop, status). [Stage 4]
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },

    /// Score a completed run from its taskstore directory. [Stage 9 body; stub parses now]
    Score {
        /// Directory containing the run's taskstore JSONL files.
        #[arg(long, short)]
        dir: PathBuf,
        /// Duration of the run in seconds (for reporting).
        #[arg(long, default_value_t = 0)]
        duration_secs: u64,
    },

    /// Inspect run logs. [Stage 2 body; stub parses now]
    Logs {
        #[command(subcommand)]
        cmd: LogsCmd,
    },

    /// List records of a given kind. [Stage 5]
    List {
        /// One of: plans, specs, phases, works, bundles, ticks.
        kind: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum LogsCmd {
    /// Show the latest run's pretty log.
    Tail {
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    /// List known runs.
    Runs,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Fork-to-daemon (default) or run in-foreground with `--foreground`.
    Start {
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the running daemon.
    Stop,
    /// Query the running daemon's status via IPC.
    Status,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use clap::CommandFactory;

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
}
