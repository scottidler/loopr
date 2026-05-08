//! Post-merge validation runner.
//!
//! `run_validation` executes each configured shell command sequentially
//! in the target directory. The first non-zero exit stops the sequence
//! and returns `ValidationError`; all commands passing returns `Ok(())`.
//!
//! Process lifetime: `.kill_on_drop(true)` is set on every `Command` so
//! that dropping the future on timeout cancellation kills the OS process.
//! `tokio::time::timeout` wraps each command independently.
//!
//! Output cap: combined stdout+stderr is truncated to `OUTPUT_CAP_BYTES`
//! (64 KiB) before being stored in `ValidationError.log`. This bounds
//! in-memory accumulation for the error record; an infinite-output command
//! is killed by the timeout, not this cap.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

const OUTPUT_CAP_BYTES: usize = 65536; // 64 KiB

#[derive(Debug)]
pub(crate) struct ValidationError {
    pub command: String,
    pub exit_code: Option<i32>,
    pub log: String,
}

pub(crate) async fn run_validation(
    commands: &[String],
    cmd_timeout: Duration,
    target: &Path,
) -> Result<(), ValidationError> {
    for cmd in commands {
        run_one(cmd, cmd_timeout, target).await?;
    }
    Ok(())
}

async fn run_one(cmd: &str, cmd_timeout: Duration, target: &Path) -> Result<(), ValidationError> {
    let result = timeout(
        cmd_timeout,
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(target)
            .kill_on_drop(true)
            .output(),
    )
    .await;

    match result {
        Err(_elapsed) => Err(ValidationError {
            command: cmd.to_string(),
            exit_code: None,
            log: format!("timed out after {:.1}s", cmd_timeout.as_secs_f64()),
        }),
        Ok(Err(io)) => Err(ValidationError {
            command: cmd.to_string(),
            exit_code: None,
            log: format!("spawn/io error: {io}"),
        }),
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => {
            let mut combined = out.stdout;
            combined.extend_from_slice(&out.stderr);
            combined.truncate(OUTPUT_CAP_BYTES);
            Err(ValidationError {
                command: cmd.to_string(),
                exit_code: out.status.code(),
                log: String::from_utf8_lossy(&combined).into_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests;
