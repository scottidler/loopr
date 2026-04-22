//! Conflict classification for failed merges.
//!
//! On merge failure, the Integrator asks: is this failure structural
//! (two Bundles touched the same file - an LLM-rescue class of
//! problem) or retryable (textual conflict, transient I/O)?
//!
//! The classifier uses `bundle.paths` (populated by the Implementer
//! from `git ls-files --modified`) rather than `work.files`
//! (LLM-predicted, often empty or inaccurate). v3 precedent
//! (`loopr/src/agents/integrator.rs:1602`).

use domain::Bundle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConflictKind {
    /// Two or more Bundles in the slice touch at least one file in
    /// common. `files` is the intersection (deduped); `peer_bundle_ids`
    /// lists every peer Bundle (by id as String) whose `paths`
    /// intersect the failing Bundle's.
    Structural {
        files: Vec<String>,
        peer_bundle_ids: Vec<String>,
    },
    /// No path overlap between the failing Bundle and any peer in the
    /// slice. The merge failure is textual / environmental; a retry
    /// with fresh state may succeed.
    Retryable,
}

/// Classify a merge failure. `failing` is the Bundle whose
/// `git merge --no-ff` exited non-zero; `peers` is the full slice
/// passed to `integrate`, including `failing` itself (the classifier
/// skips self-matches).
pub(crate) fn classify_conflict(failing: &Bundle, peers: &[Bundle]) -> ConflictKind {
    let failing_paths: std::collections::HashSet<&String> = failing.paths.iter().collect();

    let mut overlap_files: Vec<String> = Vec::new();
    let mut overlap_peers: Vec<String> = Vec::new();

    for peer in peers {
        if peer.id == failing.id {
            continue;
        }
        let mut has_any_overlap = false;
        for p in &peer.paths {
            if failing_paths.contains(p) {
                if !overlap_files.contains(p) {
                    overlap_files.push(p.clone());
                }
                has_any_overlap = true;
            }
        }
        if has_any_overlap {
            overlap_peers.push(peer.id.as_ref().to_string());
        }
    }

    if overlap_files.is_empty() {
        ConflictKind::Retryable
    } else {
        ConflictKind::Structural {
            files: overlap_files,
            peer_bundle_ids: overlap_peers,
        }
    }
}
