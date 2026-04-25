//! Per-LLM-call transcript writers.
//!
//! An LLM-using agent (Decomposer, Implementer, Reviewer) appends one
//! `TranscriptIteration` block per call. Heavy by design — system + user
//! prompt + raw response + parsed actions + dispatcher outcomes — and so
//! held outside both the structured event stream (`events.log`) and the
//! per-record summaries. Reaches: ~10 KB to multiple MB per file.
//!
//! Layout (target-local; `.git/info/exclude` covers `.loopr/records/**`):
//!
//! - `<target>/.loopr/records/plans/<plan-id>/decomposition.md`
//! - `<target>/.loopr/records/works/<work-id>/transcript.md`
//! - `<target>/.loopr/records/bundles/<bundle-id>/review.md`
//!
//! Each file is created on first append with a header block (model,
//! start timestamp, record id); subsequent appends just add iteration
//! blocks.
//!
//! Append semantics: best-effort. A failure (disk full, permission)
//! emits a `warn!` and the agent continues — a missing transcript is a
//! debug-time inconvenience, not a run-stopping error.

pub mod model;
pub mod render;

pub use model::TranscriptIteration;
pub use render::render_iteration;

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tracing::debug;

/// Per-iteration cap applied at render time. Beyond this, the truncation
/// marker `>[truncated: N KB original; sha=...]<` replaces the elided
/// region. Acceptance test asserts the literal marker.
pub const ITERATION_BYTE_CAP: usize = 100 * 1024;

/// Write a transcript iteration block to a target-local transcript file.
/// File is created on first call; subsequent calls append. Emits a
/// `tracing::debug!("transcript_appended", ...)` event after each append
/// so the raw event stream records each iteration's size without opening
/// the transcript.
pub fn append_iteration(transcript_path: &Path, iter: &TranscriptIteration) -> io::Result<()> {
    if let Some(parent) = transcript_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let block = render_iteration(iter);
    let bytes = block.as_bytes();
    let mut file = OpenOptions::new().create(true).append(true).open(transcript_path)?;
    file.write_all(bytes)?;
    debug!(
        path = %transcript_path.display(),
        iteration = iter.iteration,
        bytes = bytes.len(),
        "transcript_appended"
    );
    Ok(())
}

/// Path to a Decomposer's transcript: per-plan, single iteration.
pub fn decomposer_path(target: &Path, plan_id: &str) -> PathBuf {
    target
        .join(".loopr")
        .join("records")
        .join("plans")
        .join(plan_id)
        .join("decomposition.md")
}

/// Path to an Implementer's transcript: per-work, append-only across iterations.
pub fn implementer_path(target: &Path, work_id: &str) -> PathBuf {
    target
        .join(".loopr")
        .join("records")
        .join("works")
        .join(work_id)
        .join("transcript.md")
}

/// Path to a Reviewer's transcript: per-bundle, single iteration.
pub fn reviewer_path(target: &Path, bundle_id: &str) -> PathBuf {
    target
        .join(".loopr")
        .join("records")
        .join("bundles")
        .join(bundle_id)
        .join("review.md")
}

#[cfg(test)]
mod tests;
