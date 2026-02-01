// Phase 7: Validation System - Command Validator
// Executes shell commands to validate loop outputs

use crate::error::Result;
use crate::validation::traits::{ValidationResult, Validator};
use async_trait::async_trait;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

/// Configuration for a command validator
#[derive(Debug, Clone)]
pub struct CommandConfig {
    /// The command to execute
    pub command: String,
    /// Environment variables to set
    pub env: Vec<(String, String)>,
    /// Timeout in milliseconds (default: 30000)
    pub timeout_ms: u64,
    /// Whether to capture stderr in error messages
    pub capture_stderr: bool,
}

impl Default for CommandConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            env: Vec::new(),
            timeout_ms: 30000,
            capture_stderr: true,
        }
    }
}

impl CommandConfig {
    /// Create a new command config with the given command
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            ..Default::default()
        }
    }

    /// Add an environment variable
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Set the timeout in milliseconds
    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    /// Set whether to capture stderr
    pub fn capture_stderr(mut self, capture: bool) -> Self {
        self.capture_stderr = capture;
        self
    }
}

/// Validator that executes a shell command
pub struct CommandValidator {
    config: CommandConfig,
    name: String,
}

impl CommandValidator {
    /// Create a new command validator
    pub fn new(name: impl Into<String>, config: CommandConfig) -> Self {
        Self {
            config,
            name: name.into(),
        }
    }

    /// Create a simple command validator with defaults
    pub fn simple(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self::new(name, CommandConfig::new(command))
    }

    /// Get the validator name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the command
    pub fn command(&self) -> &str {
        &self.config.command
    }

    /// Execute the command and return the result
    async fn execute(&self, worktree: &Path) -> std::io::Result<std::process::Output> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&self.config.command);
        cmd.current_dir(worktree);

        for (key, value) in &self.config.env {
            cmd.env(key, value);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let child = cmd.spawn()?;

        // Apply timeout
        let timeout = tokio::time::Duration::from_millis(self.config.timeout_ms);
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("Command timed out after {}ms", self.config.timeout_ms),
            )),
        }
    }
}

#[async_trait]
impl Validator for CommandValidator {
    async fn validate(&self, _artifact: &Path, worktree: &Path) -> Result<ValidationResult> {
        match self.execute(worktree).await {
            Ok(output) => {
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    Ok(ValidationResult::pass_with_output(stdout.to_string()))
                } else {
                    let mut result = ValidationResult::fail(format!(
                        "Command '{}' failed with exit code: {:?}",
                        self.name,
                        output.status.code()
                    ));
                    if self.config.capture_stderr {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if !stderr.is_empty() {
                            result.add_error(format!("stderr: {}", stderr.trim()));
                        }
                    }
                    Ok(result)
                }
            }
            Err(e) => Ok(ValidationResult::fail(format!(
                "Command '{}' error: {}",
                self.name, e
            ))),
        }
    }

    fn description(&self) -> &str {
        &self.name
    }
}

/// Common command validators
pub mod presets {
    use super::*;

    /// Create a cargo check validator
    pub fn cargo_check() -> CommandValidator {
        CommandValidator::simple("cargo_check", "cargo check --all-targets")
    }

    /// Create a cargo test validator
    pub fn cargo_test() -> CommandValidator {
        CommandValidator::new(
            "cargo_test",
            CommandConfig::new("cargo test").timeout_ms(120000),
        )
    }

    /// Create a cargo clippy validator
    pub fn cargo_clippy() -> CommandValidator {
        CommandValidator::simple("cargo_clippy", "cargo clippy --all-targets -- -D warnings")
    }

    /// Create a cargo fmt check validator
    pub fn cargo_fmt() -> CommandValidator {
        CommandValidator::simple("cargo_fmt", "cargo fmt -- --check")
    }

    /// Create a custom script validator
    pub fn script(name: impl Into<String>, path: impl Into<String>) -> CommandValidator {
        CommandValidator::simple(name, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_worktree() -> PathBuf {
        PathBuf::from("/tmp")
    }

    fn test_artifact() -> PathBuf {
        PathBuf::from("/tmp/artifact.txt")
    }

    #[test]
    fn test_command_config_default() {
        let config = CommandConfig::default();
        assert!(config.command.is_empty());
        assert!(config.env.is_empty());
        assert_eq!(config.timeout_ms, 30000);
        assert!(config.capture_stderr);
    }

    #[test]
    fn test_command_config_new() {
        let config = CommandConfig::new("echo hello");
        assert_eq!(config.command, "echo hello");
    }

    #[test]
    fn test_command_config_builder() {
        let config = CommandConfig::new("test")
            .env("FOO", "bar")
            .env("BAZ", "qux")
            .timeout_ms(5000)
            .capture_stderr(false);

        assert_eq!(config.command, "test");
        assert_eq!(config.env.len(), 2);
        assert_eq!(config.env[0], ("FOO".to_string(), "bar".to_string()));
        assert_eq!(config.timeout_ms, 5000);
        assert!(!config.capture_stderr);
    }

    #[test]
    fn test_command_validator_new() {
        let config = CommandConfig::new("echo test");
        let validator = CommandValidator::new("test_cmd", config);
        assert_eq!(validator.name(), "test_cmd");
        assert_eq!(validator.command(), "echo test");
    }

    #[test]
    fn test_command_validator_simple() {
        let validator = CommandValidator::simple("echo_test", "echo hello");
        assert_eq!(validator.name(), "echo_test");
        assert_eq!(validator.command(), "echo hello");
    }

    #[tokio::test]
    async fn test_validate_success() {
        let validator = CommandValidator::simple("true_cmd", "true");
        let result = validator
            .validate(&test_artifact(), &test_worktree())
            .await
            .unwrap();
        assert!(result.passed);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn test_validate_failure() {
        let validator = CommandValidator::simple("false_cmd", "false");
        let result = validator
            .validate(&test_artifact(), &test_worktree())
            .await
            .unwrap();
        assert!(!result.passed);
        assert!(!result.errors.is_empty());
    }

    #[tokio::test]
    async fn test_validate_with_stderr() {
        let validator = CommandValidator::simple("stderr_cmd", "echo error >&2 && false");
        let result = validator
            .validate(&test_artifact(), &test_worktree())
            .await
            .unwrap();
        assert!(!result.passed);
        // Should capture stderr
        let has_stderr = result.errors.iter().any(|e| e.contains("stderr"));
        assert!(has_stderr);
    }

    #[tokio::test]
    async fn test_validate_echo_output() {
        let validator = CommandValidator::simple("echo_cmd", "echo hello");
        let result = validator
            .validate(&test_artifact(), &test_worktree())
            .await
            .unwrap();
        assert!(result.passed);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn test_validate_with_env() {
        let config = CommandConfig::new("test \"$MY_VAR\" = \"hello\"").env("MY_VAR", "hello");
        let validator = CommandValidator::new("env_cmd", config);
        let result = validator
            .validate(&test_artifact(), &test_worktree())
            .await
            .unwrap();
        assert!(result.passed);
    }

    #[tokio::test]
    async fn test_validate_timeout() {
        let config = CommandConfig::new("sleep 10").timeout_ms(100);
        let validator = CommandValidator::new("sleep_cmd", config);
        let result = validator
            .validate(&test_artifact(), &test_worktree())
            .await
            .unwrap();
        assert!(!result.passed);
        assert!(result.errors.iter().any(|e| e.contains("timed out")));
    }

    #[tokio::test]
    async fn test_validate_invalid_command() {
        let validator = CommandValidator::simple("invalid", "nonexistent_command_xyz123");
        let result = validator
            .validate(&test_artifact(), &test_worktree())
            .await
            .unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn test_preset_cargo_check() {
        let validator = presets::cargo_check();
        assert_eq!(validator.name(), "cargo_check");
        assert!(validator.command().contains("cargo check"));
    }

    #[test]
    fn test_preset_cargo_test() {
        let validator = presets::cargo_test();
        assert_eq!(validator.name(), "cargo_test");
        assert!(validator.command().contains("cargo test"));
    }

    #[test]
    fn test_preset_cargo_clippy() {
        let validator = presets::cargo_clippy();
        assert_eq!(validator.name(), "cargo_clippy");
        assert!(validator.command().contains("clippy"));
    }

    #[test]
    fn test_preset_cargo_fmt() {
        let validator = presets::cargo_fmt();
        assert_eq!(validator.name(), "cargo_fmt");
        assert!(validator.command().contains("fmt"));
    }

    #[test]
    fn test_preset_script() {
        let validator = presets::script("my_script", "./validate.sh");
        assert_eq!(validator.name(), "my_script");
        assert_eq!(validator.command(), "./validate.sh");
    }

    #[tokio::test]
    async fn test_validator_trait_description() {
        let validator = CommandValidator::simple("trait_test", "true");
        let desc: &str = Validator::description(&validator);
        assert_eq!(desc, "trait_test");
    }
}
