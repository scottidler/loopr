use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::process::ProcessId;
use crate::session::SessionId;

/// Root directory for loopr state under XDG: `$XDG_DATA_HOME/loopr` or
/// `$HOME/.local/share/loopr` if the env var is unset.
pub fn xdg_root() -> Result<PathBuf, XdgError> {
    let base = dirs::data_local_dir().ok_or(XdgError::NoDataLocalDir)?;
    Ok(base.join("loopr"))
}

/// Directory for one session under XDG:
/// `$XDG_DATA_HOME/loopr/sessions/<session-id>/`.
pub fn session_dir(session: &SessionId) -> Result<PathBuf, XdgError> {
    Ok(xdg_root()?.join("sessions").join(session.as_str()))
}

/// Directory for one target within a session:
/// `sessions/<session-id>/targets/<target-slug>/`.
pub fn session_target_dir(session: &SessionId, target_slug: &str) -> Result<PathBuf, XdgError> {
    Ok(session_dir(session)?.join("targets").join(target_slug))
}

/// Per-process run dir under XDG:
/// `sessions/<session-id>/targets/<target-slug>/runs/<process-id>/`.
/// Creates intermediate dirs as needed.
pub fn session_run_dir(session: &SessionId, target_slug: &str, process: &ProcessId) -> Result<PathBuf, XdgError> {
    let dir = session_target_dir(session, target_slug)?
        .join("runs")
        .join(process.as_str());
    std::fs::create_dir_all(&dir).map_err(|source| XdgError::CreateDir {
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

#[derive(Error, Debug)]
pub enum XdgError {
    #[error("could not resolve XDG data-local dir (HOME unset?)")]
    NoDataLocalDir,
    #[error("failed to create {path}: {source}", path = .path.display())]
    CreateDir { path: PathBuf, source: io::Error },
}
