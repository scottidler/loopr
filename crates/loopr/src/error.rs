use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum LooprError {
    #[error("target {} is a loopr source tree (sentinel found at {})", path.display(), sentinel.display())]
    SourceGuardTripped { path: PathBuf, sentinel: PathBuf },

    #[error("target {} does not exist", path.display())]
    TargetInvalid { path: PathBuf },

    #[error("target {} is a file, not a directory (try -C {} to use its parent)", path.display(), parent_hint(path))]
    TargetIsFile { path: PathBuf },

    #[error(
        "init refuses to re-root: the named target {} resolves to the enclosing git toplevel {}; \
         run init from the toplevel, or point -C directly at it",
        named.display(),
        resolved.display()
    )]
    InitTargetMismatch { named: PathBuf, resolved: PathBuf },

    #[error("log query failed: {0}")]
    LogsQuery(String),

    #[error("telemetry setup failed: {0}")]
    TelemetryInit(String),

    #[error("session resolution failed: {0}")]
    SessionResolve(String),

    #[error("ipc client error: {0}")]
    ClientIo(String),

    #[error("daemon startup failed: {0}")]
    DaemonStartup(String),

    #[error("daemon pid lock already held by another grandchild")]
    LockLost,

    #[error("handshake failed: {0}")]
    HandshakeFailed(String),

    #[error("daemon returned rpc error: {0}")]
    Rpc(#[from] ipc::RpcError),

    #[error("unknown id prefix in `{id}`; expected one of: pl-, wk-, bd-, tk-")]
    UnknownIdPrefix { id: String },

    #[error("the TUI is not built into this binary; install the `loopr-tui` crate or run `loopr <subcommand>`")]
    TuiNotInstalled,

    #[error(
        "daemon refused to start: {count} corrupt record(s) detected during sweep.\n  \
         Logs:    loopr -C <target> logs tail\n  \
         Restore: git -C <target> checkout HEAD -- .loopr/taskstore/  (taskstore is JSONL+SQLite-as-cache; JSONL is git-tracked)\n  \
         Override: re-run with --accept-corruption to start in degraded mode"
    )]
    CorruptionGate { count: usize },
}

fn parent_hint(path: &Path) -> String {
    path.parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/".to_string())
}
