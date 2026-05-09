//! Kahn's-algorithm cycle detection over the child-title dependency
//! graph, plus title-to-`WorkId` resolution.
//!
//! Ported from v3's `decomposer.rs:126-164` with one signature
//! adjustment: owned `String` keys rather than borrowed `&str`. The
//! call site constructs the map from `DecomposeChild.title` /
//! `DecomposeChild.dependencies` (both owned `String`) so trying to
//! borrow would fight the borrow checker for no performance win at
//! n <= 5.

use std::collections::HashMap;

use domain::WorkId;

use crate::error::DecomposerError;
use crate::tool::DecomposeChild;

/// Detect cycles in a dependency graph via topological sort.
///
/// `nodes` maps title -> list of dependency titles (both normalized;
/// see `normalize` in `decompose.rs`). Returns `Ok(())` if acyclic;
/// otherwise `Err(String)` with a comma-separated list of titles
/// participating in the cycle. Caller wraps into
/// `DecomposerError::CycleDetected`.
///
/// Self-loops are detected: a node whose dependency list includes
/// itself starts with `in_degree >= 1` from its self-edge and never
/// reaches zero, so Kahn's visits it zero times and the final
/// `visited < nodes.len()` check trips.
#[tracing::instrument(level = "debug", skip_all, fields(node_count = nodes.len()), err)]
pub(crate) fn detect_cycles(nodes: &HashMap<String, Vec<String>>) -> Result<(), String> {
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for title in nodes.keys() {
        in_degree.entry(title.as_str()).or_insert(0);
    }
    for deps in nodes.values() {
        for dep in deps {
            if let Some(deg) = in_degree.get_mut(dep.as_str()) {
                *deg += 1;
            }
        }
    }

    let mut queue: Vec<&str> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(title, _)| *title)
        .collect();
    let mut visited = 0usize;

    while let Some(node) = queue.pop() {
        visited += 1;
        if let Some(deps) = nodes.get(node) {
            for dep in deps {
                if let Some(deg) = in_degree.get_mut(dep.as_str()) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(dep.as_str());
                    }
                }
            }
        }
    }

    if visited < nodes.len() {
        let cycled: Vec<_> = in_degree.iter().filter(|(_, deg)| **deg > 0).map(|(t, _)| *t).collect();
        return Err(cycled.join(", "));
    }
    tracing::debug!(node_count = nodes.len(), "decomposer: detect_cycles ok");
    Ok(())
}

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
        for dep_title in &child.dependencies {
            let key = normalize(dep_title);
            match title_to_id.get(&key) {
                Some(id) => resolved.push(id.clone()),
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
