//! `loopr sessions {list, new, resume, end, status}` bodies.
//!
//! Pure filesystem operations — no daemon round-trip. Each verb reads or
//! writes the active-session pointer at `<target>/.loopr/active-session`
//! and the XDG session tree under `$XDG_DATA_HOME/loopr/sessions/`. The
//! verbs never block the caller on a live daemon so they work cleanly
//! for triage after an ungraceful daemon exit.

use std::path::Path;

use crate::cli::SessionsCmd;
use crate::error::LooprError;
use crate::session;

/// Dispatch for `loopr sessions <cmd>`. Each arm is a thin shell that
/// delegates to the `session` module helpers.
pub fn run(target: &Path, cmd: SessionsCmd) -> Result<(), LooprError> {
    match cmd {
        SessionsCmd::List => list(),
        SessionsCmd::New => new(target),
        SessionsCmd::Resume { id } => resume(target, &id),
        SessionsCmd::End => end(target),
        SessionsCmd::Status => status(target),
    }
}

/// `loopr sessions list`. Prints every session under XDG with its
/// started-at and end-state. Columns: id, started, ended-or-active.
fn list() -> Result<(), LooprError> {
    let sessions = session::list_all()?;
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    for (id, manifest) in sessions {
        let ended = match manifest.ended_at {
            Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
            None => "active".to_string(),
        };
        println!(
            "{id}  started {}  {ended}",
            manifest.started_at.format("%Y-%m-%d %H:%M:%S"),
        );
    }
    Ok(())
}

/// `loopr sessions new`. Allocates a fresh session and claims the
/// active-session pointer. Prints the new id. The invoking process's
/// own telemetry remains under whatever session it resolved at startup;
/// subsequent invocations pick up the new pointer.
fn new(target: &Path) -> Result<(), LooprError> {
    let id = session::allocate_new(target)?;
    println!("{id}");
    Ok(())
}

/// `loopr sessions resume <id>`. Validates that the id names a live
/// (not-ended) session and updates the pointer. Errors on malformed or
/// ended ids with a message that names the failure mode.
fn resume(target: &Path, id: &str) -> Result<(), LooprError> {
    let resolved = session::resolve_session_id(target, Some(id))?;
    println!("{resolved}");
    Ok(())
}

/// `loopr sessions end`. Marks the active session's manifest as ended
/// and clears the pointer. No-op (prints "no active session") if the
/// pointer is absent or names a session that was already ended.
fn end(target: &Path) -> Result<(), LooprError> {
    match session::end_active(target)? {
        Some(id) => println!("{id} ended"),
        None => println!("no active session"),
    }
    Ok(())
}

/// `loopr sessions status`. Shows the active session id, its
/// started_at, and a per-target process count. First-gate stops short
/// of querying the store for record counts — those arrive when the
/// manifest grows a records index (vision's deferred enhancement).
fn status(target: &Path) -> Result<(), LooprError> {
    let Some(id) = session::read_active(target)? else {
        println!("no active session");
        return Ok(());
    };
    let manifest = session::read_manifest(&id)?;
    println!("session:     {id}");
    println!("started-at:  {}", manifest.started_at.format("%Y-%m-%d %H:%M:%S"));
    if let Some(t) = manifest.ended_at {
        println!("ended-at:    {}", t.format("%Y-%m-%d %H:%M:%S"));
    }
    let processes = session::session_processes(&id)?;
    if processes.is_empty() {
        println!("processes:   none");
    } else {
        for (slug, pids) in processes {
            println!("target:      {slug}");
            println!("processes:   {}", pids.len());
            for pid in pids {
                println!("  - {pid}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
