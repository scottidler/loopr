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

    #[error("subcommand `{subcommand}` is not yet implemented (earned at Stage {stage})")]
    StageUnimplemented { stage: u8, subcommand: &'static str },

    #[error("log query failed: {0}")]
    LogsQuery(String),

    #[error("telemetry setup failed: {0}")]
    TelemetryInit(String),
}

fn parent_hint(path: &Path) -> String {
    path.parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/".to_string())
}
