use crate::lane::Lane;

#[derive(thiserror::Error, Debug)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("failed to serialize output: {0}")]
    SerializeOutput(String),

    #[error("sandbox violation: {0}")]
    SandboxViolation(String),

    #[error("bash command rejected by denylist: {reason}")]
    BashDenied { reason: String },

    #[error("path denied: {0}")]
    PathDenied(String),

    // NOTE (Phase-5 finding 14): there is no `Timeout` variant. A subprocess
    // timeout is NOT an error - it surfaces as `SpawnResult.timed_out: true`
    // inside an `Ok`, so callers inspect the flag (and the partial output)
    // rather than matching an error variant. See `crates/tools/CLAUDE.md`
    // "Timeout is an output flag, not an error".
    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    #[error("lane semaphore closed: {0:?}")]
    LaneClosed(Lane),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests;
