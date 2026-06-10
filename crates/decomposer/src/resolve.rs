//! Title-to-`WorkId` resolution for decomposed children, plus the title
//! `normalize` helper shared with `decompose.rs`.
//!
//! Cycle detection used to live here (Kahn's over the child-title graph)
//! but moved to `domain::WorkGraph::from_edges`, re-keyed to `WorkId`, as
//! part of the WorkGraph consolidation
//! (`docs/design/2026-05-31-workgraph-consolidation.md`). What remains is
//! decomposer-specific title handling: resolving each child's dependency
//! titles to the pre-minted sibling `WorkId`s.

use std::collections::{HashMap, HashSet};

use domain::WorkId;

use crate::error::DecomposerError;
use crate::tool::DecomposeChild;

/// Resolve each child's `dependencies` titles to the matching sibling
/// `WorkId` via `title_to_id`. Unresolved titles (the LLM named a
/// sibling that does not appear in the batch, even after case-
/// insensitive + whitespace-trimmed match) collect into a single
/// `DecomposerError::UnresolvedDeps` error naming all offenders so
/// the retry prompt can tell the model every mistake at once.
///
/// Returns one `Vec<WorkId>` per input child, in the same order as
/// `children`. `title_to_id`'s keys must already be normalized per
/// `normalize` (trim + lowercase). Each child's dependency strings
/// are normalized here before lookup.
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(child_count = children.len(), title_count = title_to_id.len()),
    err,
)]
pub(crate) fn resolve_deps(
    children: &[DecomposeChild],
    title_to_id: &HashMap<String, WorkId>,
) -> Result<Vec<Vec<WorkId>>, DecomposerError> {
    let mut resolved_per_child: Vec<Vec<WorkId>> = Vec::with_capacity(children.len());
    let mut errors: Vec<String> = Vec::new();

    for child in children {
        let mut resolved = Vec::with_capacity(child.dependencies.len());
        let mut seen: HashSet<WorkId> = HashSet::new();
        for dep_title in &child.dependencies {
            let key = normalize(dep_title);
            match title_to_id.get(&key) {
                // Dedup resolved dep ids per child: the LLM can name the
                // same sibling under two title spellings that normalize
                // to one id (or list it twice verbatim). Persisting the
                // duplicate edge would push a redundant reverse edge into
                // `WorkGraph` (which dedups defensively too, but the
                // resolved DAG should be clean at produce time per the
                // crate's "validate at produce-time" rule).
                Some(id) if seen.insert(id.clone()) => resolved.push(id.clone()),
                Some(_) => {}
                None => errors.push(format!("'{}' depends on unknown sibling '{}'", child.title, dep_title)),
            }
        }
        resolved_per_child.push(resolved);
    }

    if !errors.is_empty() {
        return Err(DecomposerError::UnresolvedDeps(errors.join("; ")));
    }
    tracing::debug!(
        child_count = children.len(),
        title_count = title_to_id.len(),
        "decomposer: resolve_deps ok"
    );
    Ok(resolved_per_child)
}

/// Canonicalize a title for case-insensitive, whitespace-insensitive
/// comparison. Used for title-to-id map keys and for the dep-graph
/// node labels so Kahn's sees the same normalized form everywhere.
pub(crate) fn normalize(title: &str) -> String {
    title.trim().to_lowercase()
}

#[cfg(test)]
mod tests;
