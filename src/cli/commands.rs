//! CLI command definitions using clap.
//!
//! Defines the main CLI structure and subcommands:
//! - daemon: start/stop/status for daemon management
//! - plan: create a new plan
//! - list: list all loops
//! - status: get loop status

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Loopr - A recursive loop-based AI task orchestrator
#[derive(Parser, Debug)]
#[command(name = "loopr")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Optional config file path
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Option<Commands>,
}

impl Cli {
    /// Check if verbose mode is enabled
    pub fn is_verbose(&self) -> bool {
        self.verbose
    }
}

/// Main subcommands
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Daemon management commands
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },

    /// Create a new plan from a task description
    Plan {
        /// Task description for the plan
        task: String,
    },

    /// List all loops
    List {
        /// Filter by status (pending, running, complete, failed)
        #[arg(short, long)]
        status: Option<String>,

        /// Show only loops of this type (plan, spec, phase, code)
        #[arg(short = 't', long)]
        loop_type: Option<String>,
    },

    /// Get status of a specific loop
    Status {
        /// Loop ID to check
        id: String,

        /// Show detailed information
        #[arg(short, long)]
        detailed: bool,
    },

    /// Approve a pending plan
    Approve {
        /// Plan ID to approve
        id: String,
    },

    /// Reject a pending plan
    Reject {
        /// Plan ID to reject
        id: String,

        /// Reason for rejection
        #[arg(short, long)]
        reason: Option<String>,
    },

    /// Pause a running loop
    Pause {
        /// Loop ID to pause
        id: String,
    },

    /// Resume a paused loop
    Resume {
        /// Loop ID to resume
        id: String,
    },

    /// Cancel/stop a loop
    Cancel {
        /// Loop ID to cancel
        id: String,
    },
}

/// Daemon management subcommands
#[derive(Subcommand, Debug, Clone)]
pub enum DaemonCommands {
    /// Start the daemon in background
    Start {
        /// Don't daemonize, run in foreground
        #[arg(short, long)]
        foreground: bool,
    },

    /// Stop the running daemon
    Stop,

    /// Check daemon status
    Status,

    /// Restart the daemon
    Restart,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    // Test helper methods for DaemonCommands
    impl DaemonCommands {
        fn is_start(&self) -> bool {
            matches!(self, DaemonCommands::Start { .. })
        }

        fn is_stop(&self) -> bool {
            matches!(self, DaemonCommands::Stop)
        }

        fn is_status(&self) -> bool {
            matches!(self, DaemonCommands::Status)
        }

        fn is_foreground(&self) -> bool {
            matches!(self, DaemonCommands::Start { foreground: true })
        }
    }

    #[test]
    fn test_cli_parse_no_args() {
        // No args should result in None command (TUI mode)
        let cli = Cli::try_parse_from(["loopr"]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.verbose);
        assert!(cli.config.is_none());
    }

    #[test]
    fn test_cli_verbose_flag() {
        let cli = Cli::try_parse_from(["loopr", "-v"]).unwrap();
        assert!(cli.is_verbose());
    }

    #[test]
    fn test_cli_config_option() {
        let cli = Cli::try_parse_from(["loopr", "-c", "/path/to/config.toml"]).unwrap();
        assert_eq!(cli.config.as_ref(), Some(&PathBuf::from("/path/to/config.toml")));
    }

    #[test]
    fn test_daemon_start() {
        let cli = Cli::try_parse_from(["loopr", "daemon", "start"]).unwrap();
        match cli.command {
            Some(Commands::Daemon {
                command: DaemonCommands::Start { foreground },
            }) => {
                assert!(!foreground);
            }
            _ => panic!("Expected daemon start command"),
        }
    }

    #[test]
    fn test_daemon_start_foreground() {
        let cli = Cli::try_parse_from(["loopr", "daemon", "start", "--foreground"]).unwrap();
        match cli.command {
            Some(Commands::Daemon { command }) => {
                assert!(command.is_foreground());
            }
            _ => panic!("Expected daemon start command"),
        }
    }

    #[test]
    fn test_daemon_stop() {
        let cli = Cli::try_parse_from(["loopr", "daemon", "stop"]).unwrap();
        match cli.command {
            Some(Commands::Daemon { command }) => {
                assert!(command.is_stop());
            }
            _ => panic!("Expected daemon stop command"),
        }
    }

    #[test]
    fn test_daemon_status() {
        let cli = Cli::try_parse_from(["loopr", "daemon", "status"]).unwrap();
        match cli.command {
            Some(Commands::Daemon { command }) => {
                assert!(command.is_status());
            }
            _ => panic!("Expected daemon status command"),
        }
    }

    #[test]
    fn test_daemon_restart() {
        let cli = Cli::try_parse_from(["loopr", "daemon", "restart"]).unwrap();
        match cli.command {
            Some(Commands::Daemon {
                command: DaemonCommands::Restart,
            }) => {}
            _ => panic!("Expected daemon restart command"),
        }
    }

    #[test]
    fn test_plan_command() {
        let cli = Cli::try_parse_from(["loopr", "plan", "Build a web server"]).unwrap();
        match cli.command {
            Some(Commands::Plan { task }) => {
                assert_eq!(task, "Build a web server");
            }
            _ => panic!("Expected plan command"),
        }
    }

    #[test]
    fn test_list_command() {
        let cli = Cli::try_parse_from(["loopr", "list"]).unwrap();
        match cli.command {
            Some(Commands::List { status, loop_type }) => {
                assert!(status.is_none());
                assert!(loop_type.is_none());
            }
            _ => panic!("Expected list command"),
        }
    }

    #[test]
    fn test_list_with_filters() {
        let cli = Cli::try_parse_from(["loopr", "list", "-s", "running", "-t", "code"]).unwrap();
        match cli.command {
            Some(Commands::List { status, loop_type }) => {
                assert_eq!(status, Some("running".to_string()));
                assert_eq!(loop_type, Some("code".to_string()));
            }
            _ => panic!("Expected list command"),
        }
    }

    #[test]
    fn test_status_command() {
        let cli = Cli::try_parse_from(["loopr", "status", "loop-123"]).unwrap();
        match cli.command {
            Some(Commands::Status { id, detailed }) => {
                assert_eq!(id, "loop-123");
                assert!(!detailed);
            }
            _ => panic!("Expected status command"),
        }
    }

    #[test]
    fn test_status_detailed() {
        let cli = Cli::try_parse_from(["loopr", "status", "loop-123", "-d"]).unwrap();
        match cli.command {
            Some(Commands::Status { id, detailed }) => {
                assert_eq!(id, "loop-123");
                assert!(detailed);
            }
            _ => panic!("Expected status command"),
        }
    }

    #[test]
    fn test_approve_command() {
        let cli = Cli::try_parse_from(["loopr", "approve", "plan-456"]).unwrap();
        match cli.command {
            Some(Commands::Approve { id }) => {
                assert_eq!(id, "plan-456");
            }
            _ => panic!("Expected approve command"),
        }
    }

    #[test]
    fn test_reject_command() {
        let cli = Cli::try_parse_from(["loopr", "reject", "plan-456"]).unwrap();
        match cli.command {
            Some(Commands::Reject { id, reason }) => {
                assert_eq!(id, "plan-456");
                assert!(reason.is_none());
            }
            _ => panic!("Expected reject command"),
        }
    }

    #[test]
    fn test_reject_with_reason() {
        let cli = Cli::try_parse_from(["loopr", "reject", "plan-456", "-r", "Not detailed enough"]).unwrap();
        match cli.command {
            Some(Commands::Reject { id, reason }) => {
                assert_eq!(id, "plan-456");
                assert_eq!(reason, Some("Not detailed enough".to_string()));
            }
            _ => panic!("Expected reject command"),
        }
    }

    #[test]
    fn test_pause_command() {
        let cli = Cli::try_parse_from(["loopr", "pause", "loop-789"]).unwrap();
        match cli.command {
            Some(Commands::Pause { id }) => {
                assert_eq!(id, "loop-789");
            }
            _ => panic!("Expected pause command"),
        }
    }

    #[test]
    fn test_resume_command() {
        let cli = Cli::try_parse_from(["loopr", "resume", "loop-789"]).unwrap();
        match cli.command {
            Some(Commands::Resume { id }) => {
                assert_eq!(id, "loop-789");
            }
            _ => panic!("Expected resume command"),
        }
    }

    #[test]
    fn test_cancel_command() {
        let cli = Cli::try_parse_from(["loopr", "cancel", "loop-789"]).unwrap();
        match cli.command {
            Some(Commands::Cancel { id }) => {
                assert_eq!(id, "loop-789");
            }
            _ => panic!("Expected cancel command"),
        }
    }

    #[test]
    fn test_help_works() {
        // Verify help doesn't panic
        Cli::command().debug_assert();
    }

    #[test]
    fn test_version_flag() {
        let result = Cli::try_parse_from(["loopr", "--version"]);
        // Version flag causes early exit with error (expected)
        assert!(result.is_err());
    }

    #[test]
    fn test_daemon_commands_helpers() {
        let start = DaemonCommands::Start { foreground: false };
        assert!(start.is_start());
        assert!(!start.is_stop());
        assert!(!start.is_status());
        assert!(!start.is_foreground());

        let start_fg = DaemonCommands::Start { foreground: true };
        assert!(start_fg.is_foreground());

        let stop = DaemonCommands::Stop;
        assert!(stop.is_stop());
        assert!(!stop.is_start());

        let status = DaemonCommands::Status;
        assert!(status.is_status());
        assert!(!status.is_start());
    }
}
