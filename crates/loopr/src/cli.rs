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
    /// Stop the running daemon (SIGTERM, escalate to SIGKILL after 3s).
    Stop,
    /// Query the running daemon's status via IPC.
    Status,
}

#[cfg(test)]
mod tests;
