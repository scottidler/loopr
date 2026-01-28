//! Layer 2: Test execution (downstream gates).
//!
//! This layer runs external commands to validate code:
//! - `cargo test` - Unit and integration tests
//! - `cargo clippy` - Linting
//! - `cargo fmt --check` - Formatting
//! - `otto ci` - All of the above (or equivalent CI command)
//!
//! Provides structured feedback with parsed error locations.

use std::path::Path;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::process::Command;

use super::feedback::{FailureCategory, FailureDetail};

/// Errors from test runner operations.
#[derive(Debug, Error)]
pub enum TestRunnerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Command failed to start: {0}")]
    CommandStart(String),
}

/// Configuration for the test runner.
#[derive(Debug, Clone)]
pub struct TestRunnerConfig {
    /// The command to run (e.g., "otto ci", "cargo test").
    pub command: String,

    /// Expected exit code for success (default: 0).
    pub success_exit_code: i32,

    /// Timeout for the command.
    pub timeout: Duration,

    /// Whether to parse output for structured errors.
    pub parse_errors: bool,
}

impl Default for TestRunnerConfig {
    fn default() -> Self {
        Self {
            command: "otto ci".to_string(),
            success_exit_code: 0,
            timeout: Duration::from_secs(300), // 5 minutes
            parse_errors: true,
        }
    }
}

impl TestRunnerConfig {
    /// Create a new config with the given command.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            ..Default::default()
        }
    }

    /// Set the expected exit code.
    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.success_exit_code = code;
        self
    }

    /// Set the timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Disable error parsing.
    pub fn without_error_parsing(mut self) -> Self {
        self.parse_errors = false;
        self
    }
}

/// Result of running tests.
#[derive(Debug, Clone)]
pub struct TestResult {
    /// Whether the tests passed.
    pub passed: bool,

    /// Exit code from the command.
    pub exit_code: Option<i32>,

    /// Parsed failure details.
    pub failures: Vec<FailureDetail>,

    /// Raw stdout output.
    pub stdout: String,

    /// Raw stderr output.
    pub stderr: String,

    /// How long the command took.
    pub duration: Duration,

    /// Whether the command timed out.
    pub timed_out: bool,
}

impl TestResult {
    /// Create a passing result.
    pub fn pass(duration: Duration) -> Self {
        Self {
            passed: true,
            exit_code: Some(0),
            failures: Vec::new(),
            stdout: String::new(),
            stderr: String::new(),
            duration,
            timed_out: false,
        }
    }

    /// Create a timeout result.
    pub fn timeout(timeout: Duration) -> Self {
        Self {
            passed: false,
            exit_code: None,
            failures: vec![FailureDetail::new(
                FailureCategory::Timeout,
                format!("Command timed out after {:?}", timeout),
            )],
            stdout: String::new(),
            stderr: String::new(),
            duration: timeout,
            timed_out: true,
        }
    }
}

/// Runner for test commands.
pub struct TestRunner {
    config: TestRunnerConfig,
}

impl TestRunner {
    /// Create a new test runner with the given config.
    pub fn new(config: TestRunnerConfig) -> Self {
        Self { config }
    }

    /// Create a test runner with default config.
    pub fn default_runner() -> Self {
        Self::new(TestRunnerConfig::default())
    }

    /// Get the command being run.
    pub fn command(&self) -> &str {
        &self.config.command
    }

    /// Run tests in the given working directory.
    pub async fn run(&self, working_dir: &Path) -> Result<TestResult, TestRunnerError> {
        let start = Instant::now();

        let output = tokio::time::timeout(
            self.config.timeout,
            Command::new("sh")
                .args(["-c", &self.config.command])
                .current_dir(working_dir)
                .output(),
        )
        .await;

        let duration = start.elapsed();

        match output {
            Ok(Ok(output)) => {
                let exit_code = output.status.code();
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                let passed = exit_code == Some(self.config.success_exit_code);

                let failures = if passed || !self.config.parse_errors {
                    Vec::new()
                } else {
                    self.parse_failures(&stdout, &stderr)
                };

                Ok(TestResult {
                    passed,
                    exit_code,
                    failures,
                    stdout,
                    stderr,
                    duration,
                    timed_out: false,
                })
            }
            Ok(Err(e)) => Err(TestRunnerError::CommandStart(e.to_string())),
            Err(_) => Ok(TestResult::timeout(self.config.timeout)),
        }
    }

    /// Parse failures from command output.
    fn parse_failures(&self, stdout: &str, stderr: &str) -> Vec<FailureDetail> {
        let mut failures = Vec::new();

        // Combine outputs for analysis
        let combined = format!("{}\n{}", stdout, stderr);

        // Parse Rust compiler errors
        failures.extend(parse_rust_errors(&combined));

        // Parse Clippy warnings
        failures.extend(parse_clippy_warnings(&combined));

        // Parse test failures
        failures.extend(parse_test_failures(&combined));

        // If we couldn't parse any structured errors, create a generic one
        if failures.is_empty() && !stderr.is_empty() {
            failures.push(
                FailureDetail::new(FailureCategory::Command, "Command failed")
                    .with_context(truncate_output(stderr, 50)),
            );
        }

        failures
    }
}

/// Parse Rust compiler errors from output.
fn parse_rust_errors(output: &str) -> Vec<FailureDetail> {
    let mut failures = Vec::new();

    for line in output.lines() {
        // Look for "error[E0XXX]:" pattern
        if line.starts_with("error[E") || line.starts_with("error:") {
            let message = line
                .strip_prefix("error:")
                .or_else(|| line.find("]: ").map(|i| &line[i + 3..]))
                .unwrap_or(line)
                .trim();

            failures.push(FailureDetail::new(FailureCategory::Type, message));
        }
    }

    // Try to associate errors with file locations
    // Pattern: " --> src/file.rs:42:10"
    let mut current_failure_idx = 0;
    for line in output.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("--> ") {
            continue;
        }

        let Some(location) = trimmed.strip_prefix("--> ") else {
            continue;
        };

        let Some((file, rest)) = location.split_once(':') else {
            continue;
        };

        let parts: Vec<&str> = rest.split(':').collect();
        let line_num = parts.first().and_then(|s| s.parse().ok());
        let col_num = parts.get(1).and_then(|s| s.parse().ok());

        if current_failure_idx < failures.len() {
            let failure = &mut failures[current_failure_idx];
            failure.file = Some(file.to_string());
            failure.line = line_num;
            failure.column = col_num;
            current_failure_idx += 1;
        }
    }

    failures
}

/// Parse Clippy warnings from output.
fn parse_clippy_warnings(output: &str) -> Vec<FailureDetail> {
    let mut failures = Vec::new();

    for line in output.lines() {
        // Look for "warning:" pattern (Clippy)
        if line.starts_with("warning:")
            && !line.contains("warning emitted")
            && !line.contains("warnings emitted")
            && !line.contains("warning(s) emitted")
        {
            let message = line.strip_prefix("warning:").unwrap_or(line).trim();

            // Skip common noise
            if message.is_empty() || message.starts_with("build failed") {
                continue;
            }

            failures.push(FailureDetail::new(FailureCategory::Lint, message));
        }
    }

    failures
}

/// Parse test failures from output.
fn parse_test_failures(output: &str) -> Vec<FailureDetail> {
    let mut failures = Vec::new();

    // Look for "test ... FAILED" pattern
    for line in output.lines() {
        if line.contains("FAILED") && line.starts_with("test ") {
            let test_name = line
                .strip_prefix("test ")
                .and_then(|s| s.split(" ...").next())
                .unwrap_or("unknown test");

            failures.push(FailureDetail::new(
                FailureCategory::Test,
                format!("Test failed: {}", test_name),
            ));
        }
    }

    // Look for assertion failures
    let mut in_assertion = false;
    let mut assertion_context = String::new();

    for line in output.lines() {
        if line.contains("assertion") && (line.contains("failed") || line.contains("panic")) {
            in_assertion = true;
            assertion_context.clear();
        }

        if in_assertion {
            assertion_context.push_str(line);
            assertion_context.push('\n');

            if line.trim().is_empty() || assertion_context.len() > 500 {
                if !assertion_context.is_empty() {
                    failures.push(
                        FailureDetail::new(FailureCategory::Test, "Assertion failed")
                            .with_context(assertion_context.trim().to_string()),
                    );
                }
                in_assertion = false;
            }
        }
    }

    failures
}

/// Truncate output to a maximum number of lines.
fn truncate_output(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().take(max_lines).collect();
    let truncated = lines.len() < output.lines().count();
    let mut result = lines.join("\n");
    if truncated {
        result.push_str("\n... (truncated)");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_test_runner_config_default() {
        let config = TestRunnerConfig::default();
        assert_eq!(config.command, "otto ci");
        assert_eq!(config.success_exit_code, 0);
        assert_eq!(config.timeout, Duration::from_secs(300));
        assert!(config.parse_errors);
    }

    #[test]
    fn test_test_runner_config_builder() {
        let config = TestRunnerConfig::new("cargo test")
            .with_exit_code(0)
            .with_timeout(Duration::from_secs(60))
            .without_error_parsing();

        assert_eq!(config.command, "cargo test");
        assert_eq!(config.timeout, Duration::from_secs(60));
        assert!(!config.parse_errors);
    }

    #[test]
    fn test_test_result_pass() {
        let result = TestResult::pass(Duration::from_secs(1));
        assert!(result.passed);
        assert!(!result.timed_out);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn test_test_result_timeout() {
        let result = TestResult::timeout(Duration::from_secs(60));
        assert!(!result.passed);
        assert!(result.timed_out);
        assert_eq!(result.failures.len(), 1);
        assert_eq!(result.failures[0].category, FailureCategory::Timeout);
    }

    #[tokio::test]
    async fn test_test_runner_success() {
        let temp = TempDir::new().unwrap();
        let config = TestRunnerConfig::new("true"); // Unix true command
        let runner = TestRunner::new(config);

        let result = runner.run(temp.path()).await.unwrap();
        assert!(result.passed);
        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_test_runner_failure() {
        let temp = TempDir::new().unwrap();
        let config = TestRunnerConfig::new("false"); // Unix false command
        let runner = TestRunner::new(config);

        let result = runner.run(temp.path()).await.unwrap();
        assert!(!result.passed);
        assert_eq!(result.exit_code, Some(1));
    }

    #[tokio::test]
    async fn test_test_runner_with_output() {
        let temp = TempDir::new().unwrap();
        let config = TestRunnerConfig::new("echo 'hello' && echo 'error' >&2 && exit 1");
        let runner = TestRunner::new(config);

        let result = runner.run(temp.path()).await.unwrap();
        assert!(!result.passed);
        assert!(result.stdout.contains("hello"));
        assert!(result.stderr.contains("error"));
    }

    #[tokio::test]
    async fn test_test_runner_timeout() {
        let temp = TempDir::new().unwrap();
        let config = TestRunnerConfig::new("sleep 10").with_timeout(Duration::from_millis(100));
        let runner = TestRunner::new(config);

        let result = runner.run(temp.path()).await.unwrap();
        assert!(!result.passed);
        assert!(result.timed_out);
    }

    #[test]
    fn test_parse_rust_errors() {
        let output = r#"
error[E0425]: cannot find value `foo` in this scope
 --> src/main.rs:10:5
  |
10 |     foo
   |     ^^^ not found in this scope

error: aborting due to previous error
"#;

        let failures = parse_rust_errors(output);
        assert!(!failures.is_empty());

        let first = &failures[0];
        assert_eq!(first.category, FailureCategory::Type);
        assert!(first.message.contains("cannot find value"));
        assert_eq!(first.file, Some("src/main.rs".to_string()));
        assert_eq!(first.line, Some(10));
        assert_eq!(first.column, Some(5));
    }

    #[test]
    fn test_parse_clippy_warnings() {
        let output = r#"
warning: unused variable: `x`
 --> src/lib.rs:5:9
  |
5 |     let x = 5;
  |         ^ help: if this is intentional, prefix it with an underscore: `_x`

warning: 1 warning emitted
"#;

        let failures = parse_clippy_warnings(output);
        assert_eq!(failures.len(), 1);

        let first = &failures[0];
        assert_eq!(first.category, FailureCategory::Lint);
        assert!(first.message.contains("unused variable"));
    }

    #[test]
    fn test_parse_test_failures() {
        let output = r#"
running 3 tests
test tests::test_a ... ok
test tests::test_b ... FAILED
test tests::test_c ... ok

failures:

---- tests::test_b stdout ----
thread 'tests::test_b' panicked at 'assertion failed: `(left == right)`
  left: `1`,
 right: `2`', src/lib.rs:10:9
"#;

        let failures = parse_test_failures(output);
        assert!(!failures.is_empty());

        let test_failure = failures.iter().find(|f| f.message.contains("test_b"));
        assert!(test_failure.is_some());
        assert_eq!(test_failure.unwrap().category, FailureCategory::Test);
    }

    #[test]
    fn test_truncate_output() {
        let output = "line1\nline2\nline3\nline4\nline5";
        let truncated = truncate_output(output, 3);
        assert!(truncated.contains("line1"));
        assert!(truncated.contains("line2"));
        assert!(truncated.contains("line3"));
        assert!(truncated.contains("truncated"));
        assert!(!truncated.contains("line4"));
    }

    #[test]
    fn test_truncate_output_no_truncation() {
        let output = "line1\nline2";
        let result = truncate_output(output, 10);
        assert_eq!(result, "line1\nline2");
        assert!(!result.contains("truncated"));
    }
}
