use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use rev_lines::RevLines;
use serde::Serialize;
use thiserror::Error;

use crate::runid::RunId;

/// One entry in `loopr logs runs`.
#[derive(Debug, Clone, Serialize)]
pub struct RunEntry {
    pub run_id: RunId,
    pub started_at: chrono::NaiveDateTime,
    pub path: PathBuf,
}

/// Read the last `n` lines of `<latest-run>/loopr.log` under
/// `<target>/.loopr/runs/`.
///
/// When the query is issued from inside a running invocation (i.e. `loopr
/// logs tail` initialized its own telemetry and allocated a run-id), callers
/// pass that id as `exclude` to keep the query from reading its own
/// still-empty log. Other callers pass `None`.
///
/// Returns `NoRunsFound` if the runs dir is empty, absent, or contains only
/// the excluded run.
pub fn tail_latest_run(target: &Path, n: usize, exclude: Option<&RunId>) -> Result<String, QueryError> {
    let runs = list_runs(target, exclude)?;
    let latest = runs.into_iter().next().ok_or_else(|| QueryError::NoRunsFound {
        path: target.join(".loopr").join("runs"),
    })?;
    let log_path = latest.path.join("loopr.log");
    let file = File::open(&log_path).map_err(|source| QueryError::Io {
        path: log_path.clone(),
        source,
    })?;
    let mut lines: Vec<String> = RevLines::new(file).filter_map(Result::ok).take(n).collect();
    lines.reverse();
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// List all run-ids under `<target>/.loopr/runs/`, newest first, with their
/// parsed started-at timestamps. Directories whose names do not parse as
/// valid RunIds are skipped silently. The `exclude` parameter suppresses a
/// caller's own in-flight run-id (see `tail_latest_run` for motivation).
pub fn list_runs(target: &Path, exclude: Option<&RunId>) -> Result<Vec<RunEntry>, QueryError> {
    let runs_dir = target.join(".loopr").join("runs");
    if !runs_dir.exists() {
        return Ok(Vec::new());
    }
    let read = std::fs::read_dir(&runs_dir).map_err(|source| QueryError::Io {
        path: runs_dir.clone(),
        source,
    })?;
    let mut entries: Vec<RunEntry> = read
        .filter_map(Result::ok)
        .filter_map(|de| {
            let file_name = de.file_name();
            let name = file_name.to_str()?;
            let run_id = RunId::parse(name).ok()?;
            if exclude.is_some_and(|e| e == &run_id) {
                return None;
            }
            let started_at = run_id.started_at()?;
            Some(RunEntry {
                run_id,
                started_at,
                path: de.path(),
            })
        })
        .collect();
    entries.sort_by(|a, b| b.run_id.as_str().cmp(a.run_id.as_str()));
    Ok(entries)
}

#[derive(Error, Debug)]
pub enum QueryError {
    #[error("no runs found at {path}", path = .path.display())]
    NoRunsFound { path: PathBuf },
    #[error("failed to read {path}: {source}", path = .path.display())]
    Io { path: PathBuf, source: io::Error },
}
