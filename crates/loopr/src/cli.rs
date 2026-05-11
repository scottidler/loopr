use std::path::PathBuf;

use clap::{Parser, Subcommand};

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

    /// Output format for data-returning verbs. Default picks JSON when
    /// stdout is a pipe, YAML when stdout is a TTY.
    #[arg(short = 'o', long = "output", global = true, value_name = "FORMAT")]
    pub output: Option<Format>,

    /// Force attach this process to the given session id. Bypasses the
    /// active-session pointer. Rejected if the session is ended.
    #[arg(long = "session", global = true, value_name = "ID")]
    pub session: Option<String>,

    /// Optional subcommand. Bare `loopr` (no subcommand) launches the TUI
    /// (post-first-gate; returns `TuiNotInstalled` until the TUI crate
    /// ships). Use `loopr tui` for the explicit form.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize loopr state at the target. Currently seeds
    /// `.loopr/prompts/` from the baked prompt tree (idempotent merge
    /// by default; `--force` overwrites). Future scope: `.loopr/`,
    /// `.loopr/taskstore/`, git hooks.
    Init {
        /// Overwrite existing `.loopr/prompts/<file>` instead of
        /// preserving them. Default mode is merge: only missing files
        /// are written.
        #[arg(long, short = 'f')]
        force: bool,
    },

    /// Submit a Plan goal to the daemon.
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

    /// Daemon lifecycle (start, stop, status).
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },

    /// Inspect run logs.
    Logs {
        #[command(subcommand)]
        cmd: LogsCmd,
    },

    /// Manage loopr sessions (list, new, resume, end, status).
    Sessions {
        #[command(subcommand)]
        cmd: SessionsCmd,
    },

    /// Director operator interaction (chat, status, ...). Phase 8 of
    /// `docs/design/2026-05-09-director-phase-2.md`.
    Director {
        #[command(subcommand)]
        cmd: DirectorCmd,
    },

    /// Launch the TUI. Same as bare `loopr` (no subcommand).
    Tui,
}

impl Command {
    /// Stable string label for telemetry and error reporting.
    pub fn label(&self) -> &'static str {
        match self {
            Command::Init { .. } => "init",
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
            Command::Sessions { cmd } => match cmd {
                SessionsCmd::List => "sessions-list",
                SessionsCmd::New => "sessions-new",
                SessionsCmd::Resume { .. } => "sessions-resume",
                SessionsCmd::End => "sessions-end",
                SessionsCmd::Status => "sessions-status",
            },
            Command::Director { cmd } => match cmd {
                DirectorCmd::Chat { .. } => "director-chat",
            },
            Command::Tui => "tui",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum DirectorCmd {
    /// Send a chat message to the Director task for the given Plan.
    /// The message is persisted as an `OperatorNote` and read by the
    /// Director's next iteration. Messages longer than 4 KB are
    /// truncated with a marker server-side.
    Chat {
        /// Target Plan id, e.g. `pl-abc12`.
        plan_id: String,
        /// One-shot message to route to the Director's user prompt.
        message: String,
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
pub enum SessionsCmd {
    /// List all known sessions under XDG.
    List,
    /// Allocate a new session and make it the active session for this target.
    New,
    /// Attach this target's active-session pointer to an existing (not-ended) session.
    Resume {
        /// Session id to attach, e.g. `20260424-150000` or `20260424-150000-2`.
        id: String,
    },
    /// Mark the active session as ended and clear this target's pointer.
    End,
    /// Show the target's active session and its processes.
    Status,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Fork-to-daemon (default) or run in-foreground with `--foreground`.
    Start {
        #[arg(long)]
        foreground: bool,

        /// Override the post-sweep corruption gate. Without this flag,
        /// the daemon refuses to bind the IPC listener if reconcile
        /// surfaced any corrupt JSONL rows; with it, the daemon emits a
        /// `warn!` and proceeds in degraded mode. Daemon-scoped: does
        /// not persist past this process's lifetime.
        #[arg(long = "accept-corruption")]
        accept_corruption: bool,
    },
    /// Stop the running daemon (SIGTERM, escalate to SIGKILL after 3s).
    Stop,
    /// Query the running daemon's status via IPC.
    Status,
}

#[cfg(test)]
mod tests;
