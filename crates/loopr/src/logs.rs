use std::path::Path;

use crate::error::LooprError;

pub fn handle_tail(target: &Path, lines: usize, exclude: Option<&telemetry::SessionId>) -> Result<(), LooprError> {
    let out =
        telemetry::tail_latest_session(target, lines, exclude).map_err(|e| LooprError::LogsQuery(e.to_string()))?;
    print!("{out}");
    Ok(())
}

pub fn handle_runs(target: &Path, exclude: Option<&telemetry::SessionId>) -> Result<(), LooprError> {
    let runs = telemetry::list_sessions(target, exclude).map_err(|e| LooprError::LogsQuery(e.to_string()))?;
    for r in runs {
        println!("{}  {}", r.session_id, r.started_at.format("%Y-%m-%d %H:%M:%S"));
    }
    Ok(())
}
