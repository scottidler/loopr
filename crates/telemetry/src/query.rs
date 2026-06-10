use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use rev_lines::RevLines;
use serde::Serialize;
use thiserror::Error;

use crate::process::ProcessId;
use crate::session::SessionId;
use crate::slug::target_slug;
use crate::xdg;

/// One entry in `loopr logs sessions`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionEntry {
    pub session_id: SessionId,
    pub started_at: chrono::NaiveDateTime,
    pub path: PathBuf,
}

/// Read the last `n` lines of the newest non-empty `loopr.log` across
/// every process-run dir for this target under XDG.
///
/// The XDG layout is:
///   `sessions/<session-id>/targets/<slug>/runs/<process-id>/loopr.log`
///
/// This function scans every `<process-id>/loopr.log` for the current
/// target across every session, picks the newest non-empty file by mtime,
/// and tails it. `exclude` is reserved for future use (the session-level
/// exclude from the pre-XDG semantics no longer applies because daemon +
/// clients now share a session).
pub fn tail_latest_session(target: &Path, n: usize, exclude_process: Option<&ProcessId>) -> Result<String, QueryError> {
    let sessions = list_sessions(target, None)?;
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in sessions {
        let runs = entry.path.join("runs");
        let Ok(reads) = std::fs::read_dir(&runs) else {
            continue;
        };
        for de in reads.flatten() {
            if let Some(name) = de.file_name().to_str()
                && exclude_process.is_some_and(|p| p.as_str() == name)
            {
                continue;
            }
            let log = de.path().join("loopr.log");
            let Ok(md) = log.metadata() else { continue };
            if md.len() == 0 {
                continue;
            }
            if let Ok(mtime) = md.modified() {
                candidates.push((mtime, log));
            }
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    let log_path = candidates
        .into_iter()
        .next()
        .map(|(_, p)| p)
        .ok_or_else(|| QueryError::NoRunsFound {
            path: target.to_path_buf(),
        })?;
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

/// List all sessions that carry activity for this target, newest-first.
/// Walks `$XDG/loopr/sessions/*/targets/<slug>/`. Directories whose names
/// don't parse as SessionIds are skipped silently.
pub fn list_sessions(target: &Path, exclude: Option<&SessionId>) -> Result<Vec<SessionEntry>, QueryError> {
    let sessions_root = xdg::xdg_root()
        .map_err(|e| QueryError::XdgRoot(e.to_string()))?
        .join("sessions");
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }
    let slug = target_slug(target).map_err(|e| QueryError::TargetSlug(e.to_string()))?;
    let read = std::fs::read_dir(&sessions_root).map_err(|source| QueryError::Io {
        path: sessions_root.clone(),
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
            let target_dir = de.path().join("targets").join(&slug);
            if !target_dir.exists() {
                return None;
            }
            let started_at = session_id.started_at()?;
            Some(SessionEntry {
                session_id,
                started_at,
                path: target_dir,
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
    #[error("xdg_root: {0}")]
    XdgRoot(String),
    #[error("target_slug: {0}")]
    TargetSlug(String),
}
