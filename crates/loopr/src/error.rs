use std::path::PathBuf;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum LooprError {
    #[error("target {} is a loopr source tree (sentinel found at {})", path.display(), sentinel.display())]
    SourceGuardTripped { path: PathBuf, sentinel: PathBuf },

    #[error("target {} does not exist or is not a directory", path.display())]
    TargetInvalid { path: PathBuf },

    #[error("subcommand `{subcommand}` is not yet implemented (earned at Stage {stage})")]
    StageUnimplemented { stage: u8, subcommand: &'static str },
}
