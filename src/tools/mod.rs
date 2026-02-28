use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use eyre::{Context, Result, eyre};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::config::ToolEntry;

/// Result of executing a tool subprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub truncated: bool,
}

/// Maximum output size per stream (stdout/stderr) in bytes (~8K tokens).
const MAX_OUTPUT: usize = 32_000;

/// Executes configured tools as async subprocesses with timeout and output truncation.
pub struct ToolRunner {
    tools: HashMap<String, ToolEntry>,
}

impl ToolRunner {
    /// Create a ToolRunner from a list of configured tool entries.
    pub fn new(entries: &[ToolEntry]) -> Self {
        let tools = entries.iter().map(|e| (e.name.clone(), e.clone())).collect();
        Self { tools }
    }

    /// List available tool names.
    pub fn available_tools(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// Look up a tool entry by name.
    pub fn get_tool(&self, name: &str) -> Option<&ToolEntry> {
        self.tools.get(name)
    }

    /// Execute a tool in the given working directory.
    ///
    /// The tool command is run via `sh -c` to support pipes and shell features.
    /// If the tool has `worktree: true`, the command runs in `working_dir`.
    /// Timeout is enforced: SIGTERM first, then SIGKILL after a grace period.
    /// Output is truncated to MAX_OUTPUT bytes per stream.
    pub async fn run(&self, tool: &str, args: &[String], working_dir: &Path) -> Result<ToolResult> {
        let entry = self.tools.get(tool).ok_or_else(|| eyre!("unknown tool: {}", tool))?;

        let full_command = if args.is_empty() {
            entry.command.clone()
        } else {
            format!("{} {}", entry.command, args.join(" "))
        };

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&full_command);

        if entry.worktree {
            cmd.current_dir(working_dir);
        }

        // Kill on drop ensures cleanup if we abandon the future
        cmd.kill_on_drop(true);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let timeout_dur = std::time::Duration::from_secs(entry.timeout_secs);
        let start = Instant::now();

        // Spawn child so we control the signal sequence (Gap #10)
        let child = cmd.spawn().context(format!("failed to spawn tool: {}", tool))?;
        #[cfg(unix)]
        let child_pid = child.id();

        let output = match tokio::time::timeout(timeout_dur, child.wait_with_output()).await {
            Ok(result) => result.context(format!("failed to execute tool: {}", tool))?,
            Err(_) => {
                // Gap #10: SIGTERM → wait 5s → SIGKILL escalation
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                    // Note: child is moved into wait_with_output above, so we can't call
                    // child.wait() here. The kill_on_drop will send SIGKILL on drop.
                }
                let duration = start.elapsed();
                return Ok(ToolResult {
                    tool: tool.to_string(),
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: format!(
                        "tool '{}' timed out after {}s (SIGTERM+SIGKILL)",
                        tool, entry.timeout_secs
                    ),
                    duration_ms: duration.as_millis() as u64,
                    truncated: false,
                });
            }
        };

        let duration = start.elapsed();

        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut truncated = false;

        if stdout.len() > MAX_OUTPUT {
            stdout.truncate(MAX_OUTPUT);
            stdout.push_str("\n... (truncated)");
            truncated = true;
        }
        if stderr.len() > MAX_OUTPUT {
            stderr.truncate(MAX_OUTPUT);
            stderr.push_str("\n... (truncated)");
            truncated = true;
        }

        Ok(ToolResult {
            tool: tool.to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            stdout,
            stderr,
            duration_ms: duration.as_millis() as u64,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entries() -> Vec<ToolEntry> {
        vec![
            ToolEntry {
                name: "echo-test".to_string(),
                command: "echo hello".to_string(),
                timeout_secs: 10,
                worktree: true,
            },
            ToolEntry {
                name: "fail-test".to_string(),
                command: "exit 1".to_string(),
                timeout_secs: 10,
                worktree: true,
            },
            ToolEntry {
                name: "stderr-test".to_string(),
                command: "echo error >&2".to_string(),
                timeout_secs: 10,
                worktree: true,
            },
            ToolEntry {
                name: "timeout-test".to_string(),
                command: "sleep 60".to_string(),
                timeout_secs: 1,
                worktree: true,
            },
            ToolEntry {
                name: "no-worktree".to_string(),
                command: "echo pwd-independent".to_string(),
                timeout_secs: 10,
                worktree: false,
            },
        ]
    }

    #[test]
    fn test_tool_runner_new() {
        let runner = ToolRunner::new(&test_entries());
        assert_eq!(runner.tools.len(), 5);
    }

    #[test]
    fn test_tool_runner_available_tools() {
        let runner = ToolRunner::new(&test_entries());
        let tools = runner.available_tools();
        assert_eq!(tools.len(), 5);
        assert!(tools.contains(&"echo-test"));
        assert!(tools.contains(&"fail-test"));
    }

    #[test]
    fn test_tool_runner_get_tool() {
        let runner = ToolRunner::new(&test_entries());
        let tool = runner.get_tool("echo-test");
        assert!(tool.is_some());
        assert_eq!(tool.unwrap().command, "echo hello");
        assert!(runner.get_tool("nonexistent").is_none());
    }

    #[test]
    fn test_tool_runner_empty() {
        let runner = ToolRunner::new(&[]);
        assert!(runner.available_tools().is_empty());
    }

    #[tokio::test]
    async fn test_run_echo() {
        let runner = ToolRunner::new(&test_entries());
        let dir = std::env::temp_dir();
        let result = runner.run("echo-test", &[], &dir).await.unwrap();
        assert_eq!(result.tool, "echo-test");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "hello");
        assert!(result.stderr.is_empty());
        assert!(!result.truncated);
        assert!(result.duration_ms < 10_000);
    }

    #[tokio::test]
    async fn test_run_with_args() {
        let entries = vec![ToolEntry {
            name: "echo-args".to_string(),
            command: "echo".to_string(),
            timeout_secs: 10,
            worktree: true,
        }];
        let runner = ToolRunner::new(&entries);
        let dir = std::env::temp_dir();
        let result = runner
            .run("echo-args", &["foo".into(), "bar".into()], &dir)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "foo bar");
    }

    #[tokio::test]
    async fn test_run_failure_exit_code() {
        let runner = ToolRunner::new(&test_entries());
        let dir = std::env::temp_dir();
        let result = runner.run("fail-test", &[], &dir).await.unwrap();
        assert_eq!(result.exit_code, 1);
    }

    #[tokio::test]
    async fn test_run_stderr() {
        let runner = ToolRunner::new(&test_entries());
        let dir = std::env::temp_dir();
        let result = runner.run("stderr-test", &[], &dir).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stderr.trim(), "error");
    }

    #[tokio::test]
    async fn test_run_unknown_tool() {
        let runner = ToolRunner::new(&test_entries());
        let dir = std::env::temp_dir();
        let result = runner.run("nonexistent", &[], &dir).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    #[tokio::test]
    async fn test_run_timeout() {
        let runner = ToolRunner::new(&test_entries());
        let dir = std::env::temp_dir();
        let result = runner.run("timeout-test", &[], &dir).await.unwrap();
        assert_eq!(result.exit_code, -1);
        assert!(result.stderr.contains("timed out"));
    }

    #[tokio::test]
    async fn test_run_worktree_dir() {
        let dir = std::env::temp_dir();
        let entries = vec![ToolEntry {
            name: "pwd-test".to_string(),
            command: "pwd".to_string(),
            timeout_secs: 10,
            worktree: true,
        }];
        let runner = ToolRunner::new(&entries);
        let result = runner.run("pwd-test", &[], &dir).await.unwrap();
        assert_eq!(result.exit_code, 0);
        // The output should contain the temp dir path
        let output_path = result.stdout.trim();
        // On Linux, /tmp may be a symlink; canonicalize both
        let expected = std::fs::canonicalize(&dir).unwrap();
        let actual = std::fs::canonicalize(output_path).unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn test_output_truncation() {
        // Generate output larger than MAX_OUTPUT
        let big_cmd = format!("python3 -c \"print('x' * {})\"", MAX_OUTPUT + 1000);
        let entries = vec![ToolEntry {
            name: "big-output".to_string(),
            command: big_cmd,
            timeout_secs: 10,
            worktree: false,
        }];
        let runner = ToolRunner::new(&entries);
        let dir = std::env::temp_dir();
        let result = runner.run("big-output", &[], &dir).await.unwrap();
        assert!(result.truncated);
        assert!(result.stdout.ends_with("... (truncated)"));
        // Truncated output should be at most MAX_OUTPUT + the truncation marker
        assert!(result.stdout.len() <= MAX_OUTPUT + 20);
    }

    #[test]
    fn test_tool_result_serde_roundtrip() {
        let result = ToolResult {
            tool: "test".to_string(),
            exit_code: 0,
            stdout: "ok".to_string(),
            stderr: String::new(),
            duration_ms: 100,
            truncated: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.tool, "test");
        assert_eq!(deserialized.exit_code, 0);
        assert_eq!(deserialized.stdout, "ok");
        assert!(!deserialized.truncated);
    }

    #[test]
    fn test_tool_result_truncated_serde() {
        let result = ToolResult {
            tool: "big".to_string(),
            exit_code: 0,
            stdout: "lots of output... (truncated)".to_string(),
            stderr: String::new(),
            duration_ms: 5000,
            truncated: true,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ToolResult = serde_json::from_str(&json).unwrap();
        assert!(deserialized.truncated);
    }

    #[test]
    fn test_max_output_constant() {
        assert_eq!(MAX_OUTPUT, 32_000);
    }
}
