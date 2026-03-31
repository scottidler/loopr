use std::path::Path;

use eyre::Result;
use log::debug;

use crate::tools::lane::Lane;
use crate::tools::router::LaneRouter;
use crate::tools::spawn::{MAX_INLINE_OUTPUT, shell_command, spawn_with_process_group};

/// Maximum output size per stream (stdout/stderr) in bytes (~8K tokens).
pub const MAX_OUTPUT: usize = MAX_INLINE_OUTPUT;

/// Result of a shell command execution with metadata.
#[derive(Debug)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub truncated: bool,
    pub timed_out: bool,
}

/// Execute a shell command in the given working directory with timeout.
///
/// Uses `setsid()` to create a new process group, enabling `killpg()` cleanup
/// on timeout (SIGTERM -> 5s grace -> SIGKILL).
///
/// This is the shared subprocess execution logic used by both `ConfiguredTool`
/// and the `ShellTool` built-in.
pub async fn execute_shell_command(command: &str, working_dir: &Path, timeout_secs: u64) -> Result<ShellOutput> {
    debug!(
        "execute_shell_command(command={}, working_dir={}, timeout={}s)",
        command,
        working_dir.display(),
        timeout_secs
    );

    let cmd = shell_command(command, working_dir);
    let result = spawn_with_process_group(cmd, timeout_secs).await?;

    Ok(ShellOutput {
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
        duration_ms: result.duration_ms,
        truncated: result.persisted_output_path.is_some(),
        timed_out: result.timed_out,
    })
}

/// Execute a shell command through the lane router with slot limiting and isolation.
pub async fn execute_in_lane(
    command: &str,
    working_dir: &Path,
    lane: Lane,
    timeout_secs: u64,
    router: &LaneRouter,
) -> Result<ShellOutput> {
    debug!(
        "execute_in_lane(lane={}, command={}, working_dir={}, timeout={}s)",
        lane,
        command,
        working_dir.display(),
        timeout_secs
    );

    let result = router.spawn(command, working_dir, lane, Some(timeout_secs)).await?;

    Ok(ShellOutput {
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
        duration_ms: result.duration_ms,
        truncated: result.persisted_output_path.is_some(),
        timed_out: result.timed_out,
    })
}

/// Format a ShellOutput into a human-readable string for tool results.
pub fn format_shell_output(output: &ShellOutput) -> String {
    let mut parts = Vec::new();

    if !output.stdout.is_empty() {
        parts.push(output.stdout.clone());
    }
    if !output.stderr.is_empty() {
        if !parts.is_empty() {
            parts.push(format!("\n--- stderr ---\n{}", output.stderr));
        } else {
            parts.push(output.stderr.clone());
        }
    }

    if parts.is_empty() {
        if output.exit_code == 0 {
            "(no output)".to_string()
        } else {
            format!("(exit code: {})", output.exit_code)
        }
    } else {
        parts.join("")
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_shell_command_echo() {
        let dir = std::env::temp_dir();
        let output = execute_shell_command("echo hello", &dir, 10).await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout.trim(), "hello");
        assert!(!output.truncated);
        assert!(!output.timed_out);
    }

    #[tokio::test]
    async fn test_execute_shell_command_failure() {
        let dir = std::env::temp_dir();
        let output = execute_shell_command("exit 1", &dir, 10).await.unwrap();
        assert_eq!(output.exit_code, 1);
    }

    #[tokio::test]
    async fn test_execute_shell_command_stderr() {
        let dir = std::env::temp_dir();
        let output = execute_shell_command("echo error >&2", &dir, 10).await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stderr.trim(), "error");
    }

    #[tokio::test]
    async fn test_execute_shell_command_timeout() {
        let dir = std::env::temp_dir();
        let output = execute_shell_command("sleep 60", &dir, 1).await.unwrap();
        assert_eq!(output.exit_code, -1);
        assert!(output.timed_out);
        assert!(output.stderr.contains("timed out"));
    }

    #[tokio::test]
    async fn test_execute_shell_command_working_dir() {
        let dir = std::env::temp_dir();
        let output = execute_shell_command("pwd", &dir, 10).await.unwrap();
        assert_eq!(output.exit_code, 0);
        let expected = std::fs::canonicalize(&dir).unwrap();
        let actual = std::fs::canonicalize(output.stdout.trim()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_format_shell_output_stdout_only() {
        let output = ShellOutput {
            stdout: "hello\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            duration_ms: 10,
            truncated: false,
            timed_out: false,
        };
        assert_eq!(format_shell_output(&output), "hello\n");
    }

    #[test]
    fn test_format_shell_output_both_streams() {
        let output = ShellOutput {
            stdout: "out".to_string(),
            stderr: "err".to_string(),
            exit_code: 0,
            duration_ms: 10,
            truncated: false,
            timed_out: false,
        };
        let formatted = format_shell_output(&output);
        assert!(formatted.contains("out"));
        assert!(formatted.contains("stderr"));
        assert!(formatted.contains("err"));
    }

    #[test]
    fn test_format_shell_output_empty_success() {
        let output = ShellOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            duration_ms: 10,
            truncated: false,
            timed_out: false,
        };
        assert_eq!(format_shell_output(&output), "(no output)");
    }

    #[test]
    fn test_format_shell_output_empty_failure() {
        let output = ShellOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 1,
            duration_ms: 10,
            truncated: false,
            timed_out: false,
        };
        assert_eq!(format_shell_output(&output), "(exit code: 1)");
    }

    #[test]
    fn test_max_output_constant() {
        assert_eq!(MAX_OUTPUT, 32_000);
    }
}
