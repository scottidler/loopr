//! Basic validation for loop iterations.
//!
//! Validation determines whether a loop iteration succeeded or needs retry.
//! This module provides command-based validation (run a command, check exit code).

use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use tokio::process::Command;

/// Validation errors
#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Validation command timed out after {0:?}")]
    Timeout(Duration),

    #[error("Failed to run validation command: {0}")]
    CommandFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for validation
#[derive(Debug, Clone)]
pub struct ValidationConfig {
    /// The command to run for validation (e.g., "otto ci", "cargo test")
    pub command: String,

    /// Expected exit code for success (default: 0)
    pub success_exit_code: i32,

    /// Timeout for the validation command
    pub timeout: Duration,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            command: "otto ci".to_string(),
            success_exit_code: 0,
            timeout: Duration::from_secs(300), // 5 minutes
        }
    }
}

impl ValidationConfig {
    /// Create a new config with the given command
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            ..Default::default()
        }
    }

    /// Set the expected exit code
    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.success_exit_code = code;
        self
    }

    /// Set the timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Result of a validation run
#[derive(Debug, Clone)]
pub enum ValidationResult {
    /// Validation passed
    Pass,

    /// Validation failed with feedback
    Fail(ValidationFeedback),
}

impl ValidationResult {
    /// Check if validation passed
    pub fn passed(&self) -> bool {
        matches!(self, ValidationResult::Pass)
    }

    /// Get the feedback if validation failed
    pub fn feedback(&self) -> Option<&ValidationFeedback> {
        match self {
            ValidationResult::Pass => None,
            ValidationResult::Fail(f) => Some(f),
        }
    }
}

/// Detailed feedback from a failed validation
#[derive(Debug, Clone)]
pub struct ValidationFeedback {
    /// Human-readable failure summary
    pub message: String,

    /// Standard output from the validation command
    pub stdout: String,

    /// Standard error from the validation command
    pub stderr: String,

    /// Exit code of the validation command
    pub exit_code: Option<i32>,

    /// Whether the validation timed out
    pub timed_out: bool,
}

impl ValidationFeedback {
    /// Create a new feedback for a command failure
    pub fn from_command_output(exit_code: Option<i32>, stdout: String, stderr: String) -> Self {
        let message = if !stderr.is_empty() {
            // Take first few lines of stderr as the message
            stderr.lines().take(5).collect::<Vec<_>>().join("\n")
        } else if !stdout.is_empty() {
            stdout.lines().take(5).collect::<Vec<_>>().join("\n")
        } else {
            format!("Validation failed with exit code {:?}", exit_code)
        };

        Self {
            message,
            stdout,
            stderr,
            exit_code,
            timed_out: false,
        }
    }

    /// Create a feedback for a timeout
    pub fn timeout() -> Self {
        Self {
            message: "Validation command timed out".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            timed_out: true,
        }
    }

    /// Format the feedback for inclusion in the next iteration prompt
    pub fn format_for_prompt(&self) -> String {
        let mut output = String::new();

        if self.timed_out {
            output.push_str("**Validation timed out**\n");
            return output;
        }

        if let Some(code) = self.exit_code {
            output.push_str(&format!("**Exit code:** {}\n", code));
        }

        if !self.stderr.is_empty() {
            output.push_str("\n**Errors:**\n```\n");
            // Limit to ~50 lines to not overwhelm the context
            let lines: Vec<&str> = self.stderr.lines().take(50).collect();
            output.push_str(&lines.join("\n"));
            if self.stderr.lines().count() > 50 {
                output.push_str("\n... (truncated)");
            }
            output.push_str("\n```\n");
        }

        if !self.stdout.is_empty() && self.stderr.is_empty() {
            output.push_str("\n**Output:**\n```\n");
            let lines: Vec<&str> = self.stdout.lines().take(50).collect();
            output.push_str(&lines.join("\n"));
            if self.stdout.lines().count() > 50 {
                output.push_str("\n... (truncated)");
            }
            output.push_str("\n```\n");
        }

        output
    }
}

/// Validator that runs commands to check loop success
pub struct Validator {
    config: ValidationConfig,
}

impl Validator {
    /// Create a new validator with the given config
    pub fn new(config: ValidationConfig) -> Self {
        Self { config }
    }

    /// Create a validator with default config
    pub fn default_validator() -> Self {
        Self::new(ValidationConfig::default())
    }

    /// Run validation in the given working directory
    pub async fn validate(&self, working_dir: &Path) -> Result<ValidationResult, ValidationError> {
        let output = tokio::time::timeout(
            self.config.timeout,
            Command::new("sh")
                .args(["-c", &self.config.command])
                .current_dir(working_dir)
                .output(),
        )
        .await;

        match output {
            Ok(Ok(output)) => {
                let exit_code = output.status.code();
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                if exit_code == Some(self.config.success_exit_code) {
                    Ok(ValidationResult::Pass)
                } else {
                    Ok(ValidationResult::Fail(ValidationFeedback::from_command_output(
                        exit_code, stdout, stderr,
                    )))
                }
            }
            Ok(Err(e)) => Err(ValidationError::CommandFailed(e.to_string())),
            Err(_) => {
                // Timeout
                Ok(ValidationResult::Fail(ValidationFeedback::timeout()))
            }
        }
    }

    /// Get the validation command
    pub fn command(&self) -> &str {
        &self.config.command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_validation_pass() {
        let temp = TempDir::new().unwrap();
        let config = ValidationConfig::new("true"); // Unix true command always succeeds
        let validator = Validator::new(config);

        let result = validator.validate(temp.path()).await.unwrap();
        assert!(result.passed());
    }

    #[tokio::test]
    async fn test_validation_fail() {
        let temp = TempDir::new().unwrap();
        let config = ValidationConfig::new("false"); // Unix false command always fails
        let validator = Validator::new(config);

        let result = validator.validate(temp.path()).await.unwrap();
        assert!(!result.passed());
    }

    #[tokio::test]
    async fn test_validation_with_output() {
        let temp = TempDir::new().unwrap();
        let config = ValidationConfig::new("echo 'test output' && exit 1");
        let validator = Validator::new(config);

        let result = validator.validate(temp.path()).await.unwrap();
        assert!(!result.passed());

        let feedback = result.feedback().unwrap();
        assert!(feedback.stdout.contains("test output"));
        assert_eq!(feedback.exit_code, Some(1));
    }

    #[tokio::test]
    async fn test_validation_with_stderr() {
        let temp = TempDir::new().unwrap();
        let config = ValidationConfig::new("echo 'error message' >&2 && exit 1");
        let validator = Validator::new(config);

        let result = validator.validate(temp.path()).await.unwrap();
        assert!(!result.passed());

        let feedback = result.feedback().unwrap();
        assert!(feedback.stderr.contains("error message"));
    }

    #[tokio::test]
    async fn test_validation_timeout() {
        let temp = TempDir::new().unwrap();
        let config = ValidationConfig::new("sleep 10").with_timeout(Duration::from_millis(100));
        let validator = Validator::new(config);

        let result = validator.validate(temp.path()).await.unwrap();
        assert!(!result.passed());

        let feedback = result.feedback().unwrap();
        assert!(feedback.timed_out);
    }

    #[test]
    fn test_validation_config_default() {
        let config = ValidationConfig::default();
        assert_eq!(config.command, "otto ci");
        assert_eq!(config.success_exit_code, 0);
        assert_eq!(config.timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_validation_config_builder() {
        let config = ValidationConfig::new("cargo test")
            .with_exit_code(0)
            .with_timeout(Duration::from_secs(60));

        assert_eq!(config.command, "cargo test");
        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_feedback_from_command_output() {
        let feedback =
            ValidationFeedback::from_command_output(Some(1), "stdout content".into(), "stderr content".into());

        assert_eq!(feedback.exit_code, Some(1));
        assert!(feedback.message.contains("stderr"));
        assert!(!feedback.timed_out);
    }

    #[test]
    fn test_feedback_timeout() {
        let feedback = ValidationFeedback::timeout();
        assert!(feedback.timed_out);
        assert!(feedback.message.contains("timed out"));
    }

    #[test]
    fn test_feedback_format_for_prompt() {
        let feedback = ValidationFeedback::from_command_output(
            Some(1),
            "some output".into(),
            "error: something failed\n  at line 42".into(),
        );

        let formatted = feedback.format_for_prompt();
        assert!(formatted.contains("Exit code:"));
        assert!(formatted.contains("Errors:"));
        assert!(formatted.contains("something failed"));
    }

    #[test]
    fn test_validation_result_passed() {
        let pass = ValidationResult::Pass;
        assert!(pass.passed());
        assert!(pass.feedback().is_none());

        let fail = ValidationResult::Fail(ValidationFeedback::timeout());
        assert!(!fail.passed());
        assert!(fail.feedback().is_some());
    }
}
