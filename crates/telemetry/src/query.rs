use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use rev_lines::RevLines;
use serde::Serialize;
use thiserror::Error;

use crate::session::SessionId;

/// One entry in `loopr logs runs`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionEntry {
    pub session_id: SessionId,
    pub started_at: chrono::NaiveDateTime,
    pub path: PathBuf,
}

/// Read the last `n` lines of `<latest-session>/loopr.log` under
/// `<target>/.loopr/runs/`.
///
/// When the query is issued from inside a running invocation (i.e. `loopr
/// logs tail` initialized its own telemetry and allocated a session-id), callers
/// pass that id as `exclude` to keep the query from reading its own
/// still-empty log. Other callers pass `None`.
///
/// Returns `NoRunsFound` if the runs dir is empty, absent, or contains only
/// the excluded session.
pub fn tail_latest_session(target: &Path, n: usize, exclude: Option<&SessionId>) -> Result<String, QueryError> {
    let runs = list_sessions(target, exclude)?;
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

/// List all session-ids under `<target>/.loopr/runs/`, newest first, with their
/// parsed started-at timestamps. Directories whose names do not parse as
/// valid SessionIds are skipped silently. The `exclude` parameter suppresses a
/// caller's own in-flight session-id (see `tail_latest_session` for motivation).
pub fn list_sessions(target: &Path, exclude: Option<&SessionId>) -> Result<Vec<SessionEntry>, QueryError> {
    let runs_dir = target.join(".loopr").join("runs");
    if !runs_dir.exists() {
        return Ok(Vec::new());
    }
    let read = std::fs::read_dir(&runs_dir).map_err(|source| QueryError::Io {
        path: runs_dir.clone(),
        source,
    })?;
    let mut entries: Vec<SessionEntry> = read
        .filter_map(Result::ok)
        .filter_map(|de| {
            let file_name = de.file_name();
            let name = file_name.to_str()?;
            let session_id = SessionId::parse(name).ok()?;
            if exclude.is_some_and(|e| e == &session_id) {
                return None;
            }
            let started_at = session_id.started_at()?;
            Some(SessionEntry {
                session_id,
                started_at,
                path: de.path(),
            })
        })
        .collect();
    entries.sort_by(|a, b| b.session_id.as_str().cmp(a.session_id.as_str()));
    Ok(entries)
}

#[derive(Error, Debug)]
pub enum QueryError {
    #[error("no runs found at {path}", path = .path.display())]
    NoRunsFound { path: PathBuf },
    #[error("failed to read {path}: {source}", path = .path.display())]
    Io { path: PathBuf, source: io::Error },
}
