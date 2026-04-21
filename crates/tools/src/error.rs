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

    #[error("timed out after {timeout_secs}s: {tool}")]
    Timeout { tool: &'static str, timeout_secs: u64 },

    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    #[error("lane semaphore closed: {0:?}")]
    LaneClosed(Lane),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests;
