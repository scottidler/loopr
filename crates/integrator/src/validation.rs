//! Post-merge validation runner.
//!
//! `run_validation` executes each configured shell command sequentially
//! in the target directory. The first non-zero exit stops the sequence
//! and returns `ValidationFailure`; all commands passing returns
//! `Ok(Vec<CommandOutcome>)`. Every command that reaches an exit code —
//! success or failure — is captured as a `CommandOutcome` so the caller
//! (`lib.rs`, Phase 12 of `docs/design/2026-07-11-verified-swarm.md`) can
//! persist a `CheckRun` per command: evidence for both green and red runs.
//! A timeout or spawn/io error never reaches an exit code (an environment
//! problem, not a check outcome — mirrors the Reviewer's Phase 10
//! spawn-error handling) and is NOT captured as a `CommandOutcome`.
//!
//! Process lifetime: `.kill_on_drop(true)` is set on every `Command` so
//! that dropping the future on timeout cancellation kills the OS process.
//! `tokio::time::timeout` wraps each command independently.
//!
//! Output cap: combined stdout+stderr is capped at `OUTPUT_CAP_BYTES`
//! (64 KiB) before being stored as the excerpt. The cap keeps a head+tail
//! split rather than the head alone: cargo/test runners put the actual
//! failure at the TAIL of their output, so a head-only truncation
//! discarded exactly the lines an operator needs. This bounds in-memory
//! accumulation for the excerpt; an infinite-output command is killed by
//! the timeout, not this cap. The digest is over the FULL combined
//! output (uncapped), so it stays a tamper-evident fingerprint even when
//! the excerpt is truncated.

use std::path::Path;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
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

/// One validation command that ran to an exit code (success or failure).
/// Never produced for a timeout/spawn-level failure. Carries everything
/// `domain::CheckRun::new` needs; the caller supplies the bundle/work
/// identity and `Role::Integrator`.
#[derive(Debug, Clone)]
pub(crate) struct CommandOutcome {
    pub command: String,
    pub exit_code: i32,
    pub output_digest: String,
    pub output_excerpt: String,
    pub duration_ms: u64,
}

/// `run_validation` failure: the triggering error, plus every
/// `CommandOutcome` the sequence produced before (and including, if it
/// completed with a nonzero exit) the failure. The caller persists a
/// `CheckRun` for each outcome regardless of the overall sequence result.
#[derive(Debug)]
pub(crate) struct ValidationFailure {
    pub error: ValidationError,
    pub outcomes: Vec<CommandOutcome>,
}

/// A single command's low-level result: either it reached an exit code
/// (`Completed`, success or failure) or it never did (`Environment`: a
/// timeout or a spawn/io error).
enum RunOneResult {
    Completed {
        outcome: CommandOutcome,
        error: Option<ValidationError>,
    },
    Environment(ValidationError),
}

pub(crate) async fn run_validation(
    commands: &[String],
    cmd_timeout: Duration,
    target: &Path,
) -> Result<Vec<CommandOutcome>, ValidationFailure> {
    let mut outcomes = Vec::with_capacity(commands.len());
    for cmd in commands {
        match run_one(cmd, cmd_timeout, target).await {
            RunOneResult::Completed { outcome, error: None } => outcomes.push(outcome),
            RunOneResult::Completed {
                outcome,
                error: Some(error),
            } => {
                outcomes.push(outcome);
                return Err(ValidationFailure { error, outcomes });
            }
            RunOneResult::Environment(error) => {
                return Err(ValidationFailure { error, outcomes });
            }
        }
    }
    Ok(outcomes)
}

#[instrument(
    name = "integrator.validation.run_one",
    level = "debug",
    skip_all,
    fields(command = cmd, timeout_ms = cmd_timeout.as_millis() as u64, elapsed_ms = tracing::field::Empty),
)]
async fn run_one(cmd: &str, cmd_timeout: Duration, target: &Path) -> RunOneResult {
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
            RunOneResult::Environment(ValidationError {
                command: cmd.to_string(),
                exit_code: None,
                log: format!("timed out after {:.1}s", cmd_timeout.as_secs_f64()),
            })
        }
        Ok(Err(io)) => {
            warn!(command = cmd, elapsed_ms, error = %io, "integrator.validation: spawn/io error");
            RunOneResult::Environment(ValidationError {
                command: cmd.to_string(),
                exit_code: None,
                log: format!("spawn/io error: {io}"),
            })
        }
        Ok(Ok(out)) => {
            let mut combined = out.stdout;
            combined.extend_from_slice(&out.stderr);
            let digest = hex_encode(&Sha256::digest(&combined));
            let excerpt = cap_head_tail(&combined, OUTPUT_CAP_BYTES);
            // `code()` is `None` only when the process was killed by a
            // signal (the timeout arm above already handles cancellation
            // via `kill_on_drop`, so a `None` here means an external
            // signal). -1 records that anomaly rather than inventing a
            // fake pass/fail code.
            let exit_code = out.status.code().unwrap_or(-1);
            let outcome = CommandOutcome {
                command: cmd.to_string(),
                exit_code,
                output_digest: digest,
                output_excerpt: excerpt.clone(),
                duration_ms: elapsed_ms,
            };
            if out.status.success() {
                debug!(command = cmd, elapsed_ms, "integrator.validation: command ok");
                RunOneResult::Completed { outcome, error: None }
            } else {
                warn!(
                    command = cmd,
                    elapsed_ms, exit_code, "integrator.validation: command failed"
                );
                let error = ValidationError {
                    command: cmd.to_string(),
                    exit_code: out.status.code(),
                    log: excerpt,
                };
                RunOneResult::Completed {
                    outcome,
                    error: Some(error),
                }
            }
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

/// Lowercase-hex encode a byte slice (avoids a `hex` crate dependency for
/// the one sha256 digest site; mirrors `agents::reviewer`'s local copy).
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests;
