//! Per-record markdown digests written alongside the typed taskstore truth.
//!
//! Per the instrumentation-sweep design doc Q4: the raw `events.log` answers
//! "every structured event in time order" but is not optimized for a reader
//! asking "what happened to record X?" Summaries are derived markdown views,
//! one file per record kind, written under `<target>/.loopr/records/<kind>/<id>/summary.md`.
//! A future reader (human or LLM agent) lands here first; transcripts and
//! events.log are linked from the summary's "Raw" section for deep-dive.
//!
//! Writes are atomic via write-to-temp + rename. Renderers are pure given
//! the input record + (optional) extra context loaded from the store. Failures
//! at the write site are best-effort: log a warn and continue. A missing or
//! stale summary is a debug-time inconvenience, not a run-stopping error.

pub mod bundle;
pub mod plan;
pub mod work;

pub use bundle::{render_bundle, write_bundle};
pub use plan::{render_plan, write_plan};
pub use work::{render_work, write_work};

use std::io;
use std::path::{Path, PathBuf};

/// Atomic write: serialize content to a temp sibling file, then `rename`
/// onto the target path. Crash-safe in that a partial write never replaces
/// the prior summary on disk.
pub(crate) fn atomic_write(target_path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = sibling_temp(target_path);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, target_path)?;
    Ok(())
}

fn sibling_temp(target_path: &Path) -> PathBuf {
    let mut s = target_path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Records-tree root under a target: `<target>/.loopr/records/`.
pub fn records_root(target: &Path) -> PathBuf {
    target.join(".loopr").join("records")
}
