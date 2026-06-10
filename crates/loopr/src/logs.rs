use std::path::Path;

use crate::error::LooprError;

#[tracing::instrument(
    name = "client.logs_tail",
    level = "info",
    skip_all,
    fields(target = %target.display(), lines, subcommand = "logs-tail"),
    err,
)]
pub fn handle_tail(
    target: &Path,
    lines: usize,
    exclude_process: Option<&telemetry::ProcessId>,
) -> Result<(), LooprError> {
    let out = telemetry::tail_latest_session(target, lines, exclude_process)
        .map_err(|e| LooprError::LogsQuery(e.to_string()))?;
    print!("{out}");
    Ok(())
}

#[tracing::instrument(
    name = "client.logs_runs",
    level = "info",
    skip_all,
    fields(target = %target.display(), subcommand = "logs-runs"),
    err,
)]
pub fn handle_runs(target: &Path, exclude_session: Option<&telemetry::SessionId>) -> Result<(), LooprError> {
    let runs = telemetry::list_sessions(target, exclude_session).map_err(|e| LooprError::LogsQuery(e.to_string()))?;
    for r in runs {
        println!("{}  {}", r.session_id, r.started_at.format("%Y-%m-%d %H:%M:%S"));
    }
    Ok(())
}
