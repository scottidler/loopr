use thiserror::Error;

/// Tool execution errors.
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("sandbox violation: {0}")]
    SandboxViolation(String),
    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),
    #[error("tool timed out after {timeout_secs}s: {tool}")]
    Timeout { tool: String, timeout_secs: u64 },
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unknown_tool_display() {
        let err = ToolError::UnknownTool("foo".to_string());
        assert_eq!(err.to_string(), "unknown tool: foo");
    }

    #[test]
    fn test_timeout_display() {
        let err = ToolError::Timeout {
            tool: "test".to_string(),
            timeout_secs: 300,
        };
        assert_eq!(err.to_string(), "tool timed out after 300s: test");
    }

    #[test]
    fn test_sandbox_violation_display() {
        let err = ToolError::SandboxViolation("path escapes sandbox".to_string());
        assert!(err.to_string().contains("sandbox violation"));
    }

    #[test]
    fn test_execution_failed_display() {
        let err = ToolError::ExecutionFailed("command not found".to_string());
        assert!(err.to_string().contains("tool execution failed"));
    }

    #[test]
    fn test_invalid_input_display() {
        let err = ToolError::InvalidInput("missing path".to_string());
        assert!(err.to_string().contains("invalid input"));
    }
}
