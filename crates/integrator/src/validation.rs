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
//! Output cap: combined stdout+stderr is capped at `OUTPUT_CAP_BYTES`
//! (64 KiB) before being stored in `ValidationError.log`. The cap keeps
//! a head+tail split rather than the head alone: cargo/test runners put
//! the actual failure at the TAIL of their output, so a head-only
//! truncation discarded exactly the lines an operator needs. This bounds
//! in-memory accumulation for the error record; an infinite-output
//! command is killed by the timeout, not this cap.

use std::path::Path;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, instrument, warn};

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

#[instrument(
    name = "integrator.validation.run_one",
    level = "debug",
    skip_all,
    fields(command = cmd, timeout_ms = cmd_timeout.as_millis() as u64, elapsed_ms = tracing::field::Empty),
)]
async fn run_one(cmd: &str, cmd_timeout: Duration, target: &Path) -> Result<(), ValidationError> {
    let started = Instant::now();
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
    let elapsed_ms = started.elapsed().as_millis() as u64;
    tracing::Span::current().record("elapsed_ms", elapsed_ms);

    match result {
        Err(_elapsed) => {
            warn!(command = cmd, elapsed_ms, "integrator.validation: command timed out");
            Err(ValidationError {
                command: cmd.to_string(),
                exit_code: None,
                log: format!("timed out after {:.1}s", cmd_timeout.as_secs_f64()),
            })
        }
        Ok(Err(io)) => {
            warn!(command = cmd, elapsed_ms, error = %io, "integrator.validation: spawn/io error");
            Err(ValidationError {
                command: cmd.to_string(),
                exit_code: None,
                log: format!("spawn/io error: {io}"),
            })
        }
        Ok(Ok(out)) if out.status.success() => {
            debug!(command = cmd, elapsed_ms, "integrator.validation: command ok");
            Ok(())
        }
        Ok(Ok(out)) => {
            let mut combined = out.stdout;
            combined.extend_from_slice(&out.stderr);
            let log = cap_head_tail(&combined, OUTPUT_CAP_BYTES);
            warn!(
                command = cmd,
                elapsed_ms,
                exit_code = ?out.status.code(),
                "integrator.validation: command failed"
            );
            Err(ValidationError {
                command: cmd.to_string(),
                exit_code: out.status.code(),
                log,
            })
        }
    }
}

/// Cap `bytes` to `cap`, keeping a head+tail split with an elision
/// marker when oversized (cargo/test failures live at the tail).
/// `from_utf8_lossy` makes the byte-boundary slices panic-safe.
fn cap_head_tail(bytes: &[u8], cap: usize) -> String {
    if bytes.len() <= cap {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let half = cap / 2;
    let head = &bytes[..half];
    let tail = &bytes[bytes.len() - half..];
    let omitted = bytes.len() - 2 * half;
    format!(
        "{}\n... [{omitted} bytes omitted] ...\n{}",
        String::from_utf8_lossy(head),
        String::from_utf8_lossy(tail)
    )
}

#[cfg(test)]
mod tests;
