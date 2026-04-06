use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eyre::{Context, Result};
use tracing::debug;

/// Maximum inline output size per stream (stdout/stderr) in bytes (~8K tokens).
pub const MAX_INLINE_OUTPUT: usize = 32_000;

/// Grace period between SIGTERM and SIGKILL during timeout escalation.
const KILL_GRACE_SECS: u64 = 5;

/// Result of spawning a tool subprocess.
#[derive(Debug, Clone)]
pub struct SpawnResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// If output exceeded MAX_INLINE_OUTPUT, the full output is written here.
    pub persisted_output_path: Option<PathBuf>,
}

/// Build a standard (non-sandboxed) shell command.
pub fn shell_command(command: &str, working_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd.current_dir(working_dir);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd
}

/// Spawn a command in its own process group with timeout and kill escalation.
///
/// The command runs in a new session via `setsid()`, making it a process group leader.
/// On timeout: `killpg(SIGTERM)` -> 5s grace -> `killpg(SIGKILL)`.
///
/// Accepts a pre-configured `Command` (from `shell_command()` or a sandbox wrapper).
pub async fn spawn_with_process_group(mut cmd: tokio::process::Command, timeout_secs: u64) -> Result<SpawnResult> {
    // Create new process group (equivalent to Node.js detached: true)
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let start = Instant::now();
    let child = cmd.spawn().context("failed to spawn command")?;
    let pgid = child.id().expect("child has PID") as i32;
    debug!("spawned child in new process group, pgid={}", pgid);

    let timeout_dur = Duration::from_secs(timeout_secs);

    match tokio::time::timeout(timeout_dur, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let duration = start.elapsed();
            debug!(
                "command completed: exit_code={}, duration={}ms",
                output.status.code().unwrap_or(-1),
                duration.as_millis()
            );
            Ok(SpawnResult::from_output(output, duration))
        }
        Ok(Err(e)) => Err(e).context("command execution failed"),
        Err(_) => {
            debug!(
                "command timed out after {}s, sending SIGTERM to pgid={}",
                timeout_secs, pgid
            );

            // Timeout: SIGTERM the process group
            #[cfg(unix)]
            unsafe {
                libc::killpg(pgid, libc::SIGTERM);
            }

            // Wait grace period for graceful shutdown, then SIGKILL.
            // The process may already be gone (ESRCH) - that's fine.
            tokio::time::sleep(Duration::from_secs(KILL_GRACE_SECS)).await;

            #[cfg(unix)]
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }

            // Note on zombie cleanup: child.wait_with_output() was consumed by the
            // timeout, which drops the inner future, which drops the Child. Tokio's
            // Child drop registers the process with a global background reaper thread
            // that quietly handles SIGCHLD. No zombies.

            Ok(SpawnResult {
                stdout: String::new(),
                stderr: format!("timed out after {}s (SIGTERM -> SIGKILL)", timeout_secs),
                exit_code: -1,
                duration_ms: start.elapsed().as_millis() as u64,
                timed_out: true,
                persisted_output_path: None,
            })
        }
    }
}

impl SpawnResult {
    fn from_output(output: std::process::Output, duration: Duration) -> Self {
        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut persisted_path = None;

        if stdout.len() > MAX_INLINE_OUTPUT {
            let full = stdout.clone();
            stdout.truncate(MAX_INLINE_OUTPUT);
            if let Some(pos) = stdout.rfind('\n') {
                stdout.truncate(pos);
            }
            match persist_output(&full) {
                Ok(path) => {
                    stdout.push_str(&format!("\n... [truncated, full output at {}]", path.display()));
                    persisted_path = Some(path);
                }
                Err(_) => {
                    stdout.push_str("\n... [truncated]");
                }
            }
        }

        Self {
            stdout,
            stderr,
            exit_code: output.status.code().unwrap_or(-1),
            duration_ms: duration.as_millis() as u64,
            timed_out: false,
            persisted_output_path: persisted_path,
        }
    }
}

/// Write large output to a temp file and return the path.
fn persist_output(content: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("loopr-tool-output");
    std::fs::create_dir_all(&dir)?;
    let filename = format!(
        "output-{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let path = dir.join(filename);
    std::fs::write(&path, content)?;
    Ok(path)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spawn_echo() {
        let cmd = shell_command("echo hello", &std::env::temp_dir());
        let result = spawn_with_process_group(cmd, 10).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "hello");
        assert!(!result.timed_out);
    }

    #[tokio::test]
    async fn test_spawn_failure() {
        let cmd = shell_command("exit 42", &std::env::temp_dir());
        let result = spawn_with_process_group(cmd, 10).await.unwrap();
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn test_spawn_stderr() {
        let cmd = shell_command("echo error >&2", &std::env::temp_dir());
        let result = spawn_with_process_group(cmd, 10).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stderr.trim(), "error");
    }

    #[tokio::test]
    async fn test_spawn_timeout_kills_process_group() {
        // Spawn a process that creates children. The entire group should be killed.
        let cmd = shell_command("sleep 60 & sleep 60 & wait", &std::env::temp_dir());
        let result = spawn_with_process_group(cmd, 1).await.unwrap();
        assert_eq!(result.exit_code, -1);
        assert!(result.timed_out);
        assert!(result.stderr.contains("timed out"));
    }

    #[tokio::test]
    async fn test_spawn_working_dir() {
        let dir = std::env::temp_dir();
        let cmd = shell_command("pwd", &dir);
        let result = spawn_with_process_group(cmd, 10).await.unwrap();
        assert_eq!(result.exit_code, 0);
        let expected = std::fs::canonicalize(&dir).unwrap();
        let actual = std::fs::canonicalize(result.stdout.trim()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_shell_command_builds_correctly() {
        let cmd = shell_command("echo test", Path::new("/tmp"));
        let inner = cmd.as_std();
        assert_eq!(inner.get_program(), "sh");
        let args: Vec<_> = inner.get_args().collect();
        assert_eq!(args, vec!["-c", "echo test"]);
    }

    #[test]
    fn test_spawn_result_from_output_small() {
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
        };
        let result = SpawnResult::from_output(output, Duration::from_millis(50));
        assert_eq!(result.stdout, "hello\n");
        assert!(result.persisted_output_path.is_none());
    }

    #[test]
    fn test_spawn_result_from_output_large_persists() {
        let big = "x".repeat(MAX_INLINE_OUTPUT + 1000);
        let output = std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: big.into_bytes(),
            stderr: Vec::new(),
        };
        let result = SpawnResult::from_output(output, Duration::from_millis(50));
        assert!(result.stdout.len() <= MAX_INLINE_OUTPUT + 200);
        assert!(result.persisted_output_path.is_some());
        // Cleanup
        if let Some(ref p) = result.persisted_output_path {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn test_persist_output() {
        let path = persist_output("test content").unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "test content");
        let _ = std::fs::remove_file(&path);
    }
}
