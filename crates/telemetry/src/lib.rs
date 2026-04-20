//! Observability for loopr-v5.
//!
//! # Instrumentation conventions
//!
//! Use raw `#[tracing::instrument(level = "...", skip_all, fields(...))]` at
//! the emission site. Don't hand-roll `span_for_stage!` / `span_for_ralph!` /
//! `span_for_tool!` macros - those are an earned upgrade if we ever cross the
//! ≥5x copy-pasted-`fields(...)` threshold. Until then, the
//! instrument-attribute form is the v5 standard and the instrumentation-
//! conventions subsection of `docs/design/2026-04-19-telemetry-stage-2.md`
//! is its permanent contract.

mod fanout;
mod query;
mod runid;
mod subscriber;

/// Environment variable that shadows the CLI `--log-level` flag default.
pub const LOG_ENV_VAR: &str = "LOOPR_LOG_LEVEL";

/// `stage.<name>` - stage-boundary spans (`stage.plan`, `stage.decompose`, ...)
pub const STAGE_PREFIX: &str = "stage";
/// `ralph.<role>` - ralph-loop spans (`ralph.implementer`, `ralph.reviewer`, ...)
pub const RALPH_PREFIX: &str = "ralph";
/// `tool.<name>` - tool-invocation spans (`tool.bash`, `tool.edit`, ...)
pub const TOOL_PREFIX: &str = "tool";

pub use fanout::WorkFanoutLayer;
pub use query::{QueryError, RunEntry, list_runs, tail_latest_run};
pub use runid::{RunId, RunIdAllocError, RunIdParseError};
pub use subscriber::{Guard, SharedWriter, SharedWriterGuard, TelemetryInitError, init};

#[cfg(test)]
mod tests;
