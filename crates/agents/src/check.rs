//! `CheckRunner`: executes the configured `reviewer.check-commands` in a
//! Bundle's checkout and returns typed outcomes.
//!
//! Phase 10 of `docs/design/2026-07-11-verified-swarm.md`. The Reviewer runs
//! these checks BEFORE the LLM turn and code-gates the verdict on their exit
//! codes: the LLM never overrides a red check. Persisted evidence is a
//! `domain::CheckRun` per command (written by `run_reviewer`).
//!
//! ## Failure taxonomy (the load-bearing distinction)
//!
//! Each command is run by spawning its program DIRECTLY (argv[0] + args), not
//! via `sh -c`, so the check tool itself sits at the OS spawn boundary:
//!
//! - **Spawn-level failure** (`spawn_error: Some`): the program could not be
//!   located or `fork`/`exec` failed (`command not found`). This is an
//!   ENVIRONMENT problem, not a code problem — the Reviewer maps it to a
//!   Blocked Work with no LLM turn. Asking the LLM to fix infra burns
//!   `max_work_attempts` at max cost.
//! - **Clean spawn, nonzero exit** (`spawn_error: None`, `exit_code != 0`): a
//!   CODE signal. The deterministic accept gate overrides an LLM `Accept` to
//!   `ChangeRequested`.
//! - **Clean spawn, zero exit**: green.
//!
//! Execution reuses the tools crate's heavy-lane spawn infrastructure
//! (`LaneRouter::spawn` with `Lane::Heavy`): existing wall-clock timeout,
//! bounded/persisted output, process-group kill on timeout.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use tracing::{debug, warn};
use uuid::Uuid;

use tools::{Lane, LaneRouter, PersistConfig};

/// One executed (or attempted) check command and its outcome.
///
/// `spawn_error` is the sole discriminant between an environment failure and a
/// code signal: `Some` means the process never ran (spawn boundary failure);
/// `None` means it ran and `exit_code` is authoritative.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    /// The command as configured / executed (argv joined for display).
    pub command: String,
    /// Process exit code. `0` = green; nonzero = red (code signal). `-1` is
    /// the placeholder when `spawn_error` is set (the process never ran).
    pub exit_code: i32,
    /// Combined stdout+stderr, already bounded by the tools spawn layer's
    /// `MAX_INLINE_OUTPUT`. Empty when `spawn_error` is set.
    pub combined_output: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// `Some(detail)` iff the spawn itself failed (program not found / exec
    /// failure). Environment problem -> Blocked, no LLM turn.
    pub spawn_error: Option<String>,
}

impl CheckOutcome {
    /// A command produced a clean spawn with a nonzero exit code: a code
    /// signal the deterministic accept gate acts on.
    pub fn is_red(&self) -> bool {
        self.spawn_error.is_none() && self.exit_code != 0
    }
}

/// Executes check commands against a checkout and returns one outcome per
/// command. Named as `Arc<dyn CheckRunner>` in `ReviewerDeps` per the design
/// doc's API Design; the boxed-future signature keeps the trait
/// dyn-compatible without pulling in `async_trait`.
pub trait CheckRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        checkout_path: &'a Path,
        commands: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Vec<CheckOutcome>> + Send + 'a>>;
}

/// Production `CheckRunner`: shells each command through the tools crate's
/// heavy-lane spawn path (`LaneRouter::spawn`), inheriting its timeout,
/// output bounding, and process-group kill.
pub struct ProductionCheckRunner {
    router: Arc<LaneRouter>,
    persist_base: Option<PathBuf>,
}

impl ProductionCheckRunner {
    pub fn new(router: Arc<LaneRouter>, persist_base: Option<PathBuf>) -> Self {
        Self { router, persist_base }
    }
}

impl CheckRunner for ProductionCheckRunner {
    fn run<'a>(
        &'a self,
        checkout_path: &'a Path,
        commands: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Vec<CheckOutcome>> + Send + 'a>> {
        Box::pin(async move {
            let mut outcomes = Vec::with_capacity(commands.len());
            for command in commands {
                outcomes.push(self.run_one(checkout_path, command).await);
            }
            outcomes
        })
    }
}

impl ProductionCheckRunner {
    async fn run_one(&self, checkout_path: &Path, command: &str) -> CheckOutcome {
        debug!(command = %command, checkout = %checkout_path.display(), "check: running");

        // Tokenize the command into argv and spawn the program directly, so a
        // missing tool surfaces as a genuine OS spawn error (the env-vs-code
        // boundary) rather than a shell's 127 exit code.
        let argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
        let Some((program, args)) = argv.split_first() else {
            return CheckOutcome {
                command: command.to_string(),
                exit_code: -1,
                combined_output: String::new(),
                duration_ms: 0,
                spawn_error: Some("empty check command".to_string()),
            };
        };

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        cmd.current_dir(checkout_path);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let persist = PersistConfig {
            base: self.persist_base.as_deref(),
            invocation_id: Some(Uuid::now_v7()),
        };

        match self.router.spawn(cmd, Lane::Heavy, checkout_path, None, persist).await {
            Ok(result) => CheckOutcome {
                command: command.to_string(),
                exit_code: result.exit_code,
                combined_output: result.combined_output,
                duration_ms: result.duration_ms,
                spawn_error: None,
            },
            Err(e) => {
                // `LaneRouter::spawn` only errors on a spawn-level failure
                // (program not found / exec failure) or a closed lane; both
                // are environment problems, not code signals.
                warn!(command = %command, error = %e, "check: spawn-level failure (environment)");
                CheckOutcome {
                    command: command.to_string(),
                    exit_code: -1,
                    combined_output: String::new(),
                    duration_ms: 0,
                    spawn_error: Some(e.to_string()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
