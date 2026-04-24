use std::path::PathBuf;

use clap::{Parser, Subcommand};

use worktree::AttemptCleanupPolicy;

use crate::output::Format;

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

    /// Set the tracing filter directive. Env: LOOPR_LOG_LEVEL. Default: info.
    ///
    /// Accepts any `EnvFilter`-parseable string:
    ///   --log-level debug                           (bare level, all targets)
    ///   --log-level loopr=debug,tools=error         (per-target directives)
    ///   --log-level warn,loopr::agents=trace        (combination)
    ///   --log-level off                             (silence everything)
    #[arg(short = 'l', long = "log-level", global = true, value_name = "DIRECTIVE")]
    pub log_level: Option<String>,

    /// When the coordinator cleans up per-attempt worktrees. Overrides
    /// `.loopr/config.yml` and `LOOPR_WORKTREE_CLEANUP_POLICY`.
    ///
    /// - immediate: clean on Bundle rejection (smallest disk; no forensics)
    /// - on-work-terminal: sweep prior attempts when Work reaches Done/Abandoned (DEFAULT)
    /// - on-run-end: keep everything until the run ends, then sweep
    /// - never: strict debug-only; leaks memory + FDs on long-uptime daemons
    #[arg(long = "worktree-cleanup", global = true, value_name = "POLICY")]
    pub worktree_cleanup: Option<AttemptCleanupPolicy>,

    /// Output format for data-returning verbs. Default picks JSON when
    /// stdout is a pipe, YAML when stdout is a TTY.
    #[arg(short = 'o', long = "output", global = true, value_name = "FORMAT")]
    pub output: Option<Format>,

    /// Optional subcommand. Bare `loopr` (no subcommand) launches the TUI
    /// (post-first-gate; returns `NotYetImplemented` until the TUI crate
    /// ships). Use `loopr tui` for the explicit form.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize loopr state (.loopr/, .loopr/taskstore/, git hooks) at the target. [Stage 5]
    Init,

    /// Submit a Plan goal to the daemon. [Stage 5]
    Plan {
        /// One-sentence goal to plan for.
        goal: String,
    },

    /// List all Plans in the target's taskstore.
    Plans,

    /// List all Work items in the target's taskstore.
    Works,

    /// List all Bundles in the target's taskstore.
    Bundles,

    /// List all Ticks in the target's taskstore.
    Ticks,

    /// Show a single record by ID (prefix-routed: pl-/wk-/bd-/tk-).
    Show {
        /// Record ID. The 2-char prefix picks the record type.
        id: String,
    },

    /// Daemon lifecycle (start, stop, status). [Stage 4]
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },

    /// Inspect run logs. [Stage 2 body; stub parses now]
    Logs {
        #[command(subcommand)]
        cmd: LogsCmd,
    },

    /// Launch the TUI. Same as bare `loopr` (no subcommand).
    Tui,
}

impl Command {
    /// Stable string label for telemetry and error reporting.
    pub fn label(&self) -> &'static str {
        match self {
            Command::Init => "init",
            Command::Plan { .. } => "plan",
            Command::Plans => "plans",
            Command::Works => "works",
            Command::Bundles => "bundles",
            Command::Ticks => "ticks",
            Command::Show { .. } => "show",
            Command::Daemon { cmd } => match cmd {
                DaemonCmd::Start { .. } => "daemon-start",
                DaemonCmd::Stop => "daemon-stop",
                DaemonCmd::Status => "daemon-status",
            },
            Command::Logs { cmd } => match cmd {
                LogsCmd::Tail { .. } => "logs-tail",
                LogsCmd::Runs => "logs-runs",
            },
            Command::Tui => "tui",
        }
    }
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
    /// Stop the running daemon (SIGTERM, escalate to SIGKILL after 3s).
    Stop,
    /// Query the running daemon's status via IPC.
    Status,
}

#[cfg(test)]
mod tests;
