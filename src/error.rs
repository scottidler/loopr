//! Error types for Loopr
//!
//! Centralized error handling using thiserror.

use thiserror::Error;

/// All error types that can occur in Loopr
#[derive(Debug, Error)]
pub enum LooprError {
    /// Loop not found in storage
    #[error("Loop not found: {0}")]
    LoopNotFound(String),

    /// Invalid state transition or operation
    #[error("Invalid state: {0}")]
    InvalidState(String),

    /// Validation of output failed
    #[error("Validation failed: {0}")]
    ValidationFailed(String),

    /// Storage/persistence error
    #[error("Storage error: {0}")]
    Storage(String),

    /// LLM API error
    #[error("LLM error: {0}")]
    Llm(String),

    /// Tool execution error
    #[error("Tool error: {0}")]
    Tool(String),

    /// Git worktree error
    #[error("Worktree error: {0}")]
    Worktree(String),

    /// IPC communication error
    #[error("IPC error: {0}")]
    Ipc(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result type alias for Loopr operations
pub type Result<T> = std::result::Result<T, LooprError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_not_found_error() {
        let err = LooprError::LoopNotFound("001".to_string());
        assert_eq!(err.to_string(), "Loop not found: 001");
    }

    #[test]
    fn test_invalid_state_error() {
        let err = LooprError::InvalidState("cannot pause completed loop".to_string());
        assert_eq!(err.to_string(), "Invalid state: cannot pause completed loop");
    }

    #[test]
    fn test_validation_failed_error() {
        let err = LooprError::ValidationFailed("missing ## Overview section".to_string());
        assert_eq!(err.to_string(), "Validation failed: missing ## Overview section");
    }

    #[test]
    fn test_storage_error() {
        let err = LooprError::Storage("file locked".to_string());
        assert_eq!(err.to_string(), "Storage error: file locked");
    }

    #[test]
    fn test_llm_error() {
        let err = LooprError::Llm("rate limited".to_string());
        assert_eq!(err.to_string(), "LLM error: rate limited");
    }

    #[test]
    fn test_tool_error() {
        let err = LooprError::Tool("timeout".to_string());
        assert_eq!(err.to_string(), "Tool error: timeout");
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: LooprError = io_err.into();
        assert!(matches!(err, LooprError::Io(_)));
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn test_json_error_conversion() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err: LooprError = json_err.into();
        assert!(matches!(err, LooprError::Json(_)));
    }

    #[test]
    fn test_result_type_alias() {
        fn returns_ok() -> Result<i32> {
            Ok(42)
        }

        fn returns_err() -> Result<i32> {
            Err(LooprError::InvalidState("test".to_string()))
        }

        assert!(returns_ok().is_ok());
        assert!(returns_err().is_err());
    }
}
