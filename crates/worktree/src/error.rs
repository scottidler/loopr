use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("git command failed: {0}")]
    GitCommand(String),

    #[error("worktree not found at {0}")]
    NotFound(PathBuf),

    #[error("failed to allocate seq after {attempts} attempts under {dir}")]
    SeqAllocExhausted { attempts: u32, dir: PathBuf },

    #[error("invalid branch name {0:?}: not a loopr-managed branch")]
    InvalidBranchName(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests;
