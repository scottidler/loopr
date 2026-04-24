//! Session resolution + active-session pointer + minimal manifest.
//!
//! Every loopr process resolves a `SessionId` at startup before the
//! telemetry subscriber initializes (Phase 5). Resolution rules:
//!
//!   1. If `--session <id>` provided, validate it exists and is not ended;
//!      update the pointer to attach.
//!   2. If `<target>/.loopr/active-session` exists and points at a valid
//!      (not-ended) session, use it.
//!   3. Otherwise allocate a new session and claim the pointer atomically.
//!
//! Allocation uses `SessionId::allocate` for the session-id itself (EEXIST
//! race on `.loopr/runs/<id>/`; Phase 5 moves this to XDG). Pointer claim
//! uses `O_CREAT | O_EXCL` on `<target>/.loopr/active-session`. Under 50
//! concurrent clients with no existing pointer, all 50 allocate distinct
//! session-ids but only ONE wins the pointer race; the 49 losers re-read
//! the pointer and return the winner's id.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use telemetry::SessionId;

use crate::error::LooprError;

/// Filename of the per-target active-session pointer.
pub const POINTER_FILENAME: &str = "active-session";

/// Absolute cap on resolver retry iterations. Under realistic contention
/// each iteration either returns (valid pointer) or loses a pointer-claim
/// race and observes the winner's id on the next read; more than a handful
/// of retries indicates a bug (corrupt pointer, clock skew, pathological
/// EEXIST storm) worth surfacing rather than spinning forever.
const MAX_RESOLVE_RETRIES: u32 = 100;

/// Minimal session manifest. Phase 8 will extend this (targets[], processes[],
/// records[]); Phase 4 needs only the three fields that let the resolver
/// detect an ended session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionManifest {
    pub session_id: String,
    pub started_at: chrono::DateTime<chrono::Local>,
    pub ended_at: Option<chrono::DateTime<chrono::Local>>,
}

/// Resolve the session-id for this process.
///
/// `flag`: value of the `--session <id>` CLI flag, if any.
///
/// Ordering:
///   1. `flag.is_some()` -> validate + attach, return the validated id.
///   2. Read `<target>/.loopr/active-session`. If it points at a
///      live (not-ended) session, return that id.
///   3. Otherwise allocate a new session: allocate a SessionId, write
///      the manifest under XDG, atomically claim the pointer. On claim
///      loss, re-read the pointer (the winner wrote it).
pub fn resolve_session_id(target: &Path, flag: Option<&str>) -> Result<SessionId, LooprError> {
    if let Some(s) = flag {
        let id =
            SessionId::parse(s).map_err(|e| LooprError::SessionResolve(format!("bad --session value `{s}`: {e}")))?;
        if session_ended(&id)? {
            return Err(LooprError::SessionResolve(format!("session {id} is ended")));
        }
        attach_pointer(target, &id)?;
        return Ok(id);
    }
    let pointer = pointer_path(target);
    for _ in 0..MAX_RESOLVE_RETRIES {
        match read_pointer_state(&pointer)? {
            PointerState::Live(id) => return Ok(id),
            PointerState::Stale => {
                // Stale pointer (corrupt or ended). Best-effort remove so
                // the next iteration's O_CREAT|O_EXCL can succeed. If
                // another process already removed it, ignore.
                let _ = fs::remove_file(&pointer);
            }
            PointerState::Absent => {}
        }
        let candidate = allocate_new_session()?;
        match claim_pointer_exclusive(&pointer, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(LooprError::SessionResolve(format!(
                    "pointer claim {}: {e}",
                    pointer.display()
                )));
            }
        }
    }
    Err(LooprError::SessionResolve(format!(
        "exhausted {MAX_RESOLVE_RETRIES} resolve retries"
    )))
}

/// Path to `<target>/.loopr/active-session`.
pub fn pointer_path(target: &Path) -> PathBuf {
    target.join(".loopr").join(POINTER_FILENAME)
}

/// Observed state of the active-session pointer, in enough detail for the
/// resolver to know whether to fall through (absent), remove (stale), or
/// return (live).
enum PointerState {
    /// File does not exist.
    Absent,
    /// File exists but doesn't name a live session (corrupt id, or manifest
    /// marks `ended_at`). Caller should `remove_file` and retry.
    Stale,
    /// File exists and names a session whose manifest has `ended_at: null`.
    Live(SessionId),
}

fn read_pointer_state(pointer: &Path) -> Result<PointerState, LooprError> {
    match fs::read_to_string(pointer) {
        Ok(s) => match SessionId::parse(s.trim()) {
            Ok(id) => {
                if session_ended(&id)? {
                    Ok(PointerState::Stale)
                } else {
                    Ok(PointerState::Live(id))
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %pointer.display(), "active-session pointer corrupt; treating as stale");
                Ok(PointerState::Stale)
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(PointerState::Absent),
        Err(e) => Err(LooprError::SessionResolve(format!("read {}: {e}", pointer.display()))),
    }
}

/// Atomic exclusive claim of the pointer file with `session_id` as content.
/// Returns `Err(AlreadyExists)` if another process/thread claimed it first.
///
/// Two-step hard-link protocol, required because pairing
/// `OpenOptions::create_new` with a subsequent `writeln!` is NOT atomic
/// from a reader's perspective: between the atomic `O_CREAT|O_EXCL` and
/// the subsequent `writeln`, a concurrent reader sees an empty pointer
/// file, parses it as corrupt, and removes it, racing a third process
/// back into an `Absent` state.
///
/// Instead:
///   1. Write the final content to a per-thread temp file in the same dir.
///   2. `hard_link(temp, pointer)`: POSIX `link(2)` fails with `EEXIST` if
///      the destination already exists, atomically from the reader's
///      perspective. The pointer either doesn't exist, or it exists with
///      full content.
///   3. Remove temp (best-effort).
fn claim_pointer_exclusive(pointer: &Path, session_id: &SessionId) -> std::io::Result<()> {
    let parent = pointer
        .parent()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "pointer path has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".active-session.tmp.{}.{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        writeln!(f, "{}", session_id.as_str())?;
        f.sync_all()?;
    }
    let result = fs::hard_link(&tmp, pointer);
    let _ = fs::remove_file(&tmp);
    result
}

/// Write pointer via write-to-temp + rename. Used by the `--session` flag
/// attach path where we intentionally replace an existing pointer.
fn attach_pointer(target: &Path, session_id: &SessionId) -> Result<(), LooprError> {
    let pointer = pointer_path(target);
    if let Some(parent) = pointer.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| LooprError::SessionResolve(format!("mkdir {}: {e}", parent.display())))?;
    }
    let tmp = pointer.with_extension("tmp");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| LooprError::SessionResolve(format!("open tmp {}: {e}", tmp.display())))?;
        writeln!(f, "{}", session_id.as_str())
            .map_err(|e| LooprError::SessionResolve(format!("write tmp {}: {e}", tmp.display())))?;
        f.sync_all()
            .map_err(|e| LooprError::SessionResolve(format!("fsync tmp {}: {e}", tmp.display())))?;
    }
    fs::rename(&tmp, &pointer)
        .map_err(|e| LooprError::SessionResolve(format!("rename {} -> {}: {e}", tmp.display(), pointer.display())))
}

/// Allocate a new session by atomically claiming a timestamped directory
/// under XDG `sessions/`. The `create_dir` EEXIST race inside
/// `SessionId::allocate` is the atomicity anchor: on collision, a `-N`
/// suffix is appended. Writes the Phase-4 manifest inside the claimed dir
/// before returning.
///
/// Phase-5 note: the claim anchor moved from `<target>/.loopr/runs/`
/// (which no longer exists for allocation purposes) to the user-global
/// XDG `sessions/` tree.
fn allocate_new_session() -> Result<SessionId, LooprError> {
    let sessions_root = telemetry::xdg_root()
        .map_err(|e| LooprError::SessionResolve(format!("xdg_root: {e}")))?
        .join("sessions");
    fs::create_dir_all(&sessions_root)
        .map_err(|e| LooprError::SessionResolve(format!("mkdir {}: {e}", sessions_root.display())))?;
    let candidate = SessionId::allocate(&sessions_root)
        .map_err(|e| LooprError::SessionResolve(format!("session id alloc: {e}")))?;
    let dir = sessions_root.join(candidate.as_str());
    let manifest = SessionManifest {
        session_id: candidate.as_str().to_string(),
        started_at: chrono::Local::now(),
        ended_at: None,
    };
    let body =
        serde_yaml::to_string(&manifest).map_err(|e| LooprError::SessionResolve(format!("serialize manifest: {e}")))?;
    let manifest_path = dir.join("manifest.yml");
    fs::write(&manifest_path, body)
        .map_err(|e| LooprError::SessionResolve(format!("write {}: {e}", manifest_path.display())))?;
    Ok(candidate)
}

/// Returns `true` if the session's manifest exists and has a non-null
/// `ended_at`. A missing manifest is not "ended" — it could be an
/// in-flight allocation we're racing with. Only an explicit `ended_at`
/// set counts as ended.
fn session_ended(session_id: &SessionId) -> Result<bool, LooprError> {
    let dir =
        telemetry::session_dir(session_id).map_err(|e| LooprError::SessionResolve(format!("xdg session_dir: {e}")))?;
    let manifest_path = dir.join("manifest.yml");
    match fs::read_to_string(&manifest_path) {
        Ok(body) => {
            let manifest: SessionManifest = serde_yaml::from_str(&body)
                .map_err(|e| LooprError::SessionResolve(format!("parse {}: {e}", manifest_path.display())))?;
            Ok(manifest.ended_at.is_some())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(LooprError::SessionResolve(format!(
            "read {}: {e}",
            manifest_path.display()
        ))),
    }
}

#[cfg(test)]
mod tests;
