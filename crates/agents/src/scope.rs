//! Filter `git status --porcelain` output by Work scope.
//!
//! Two functions:
//! - `parse_porcelain_status`: porcelain text into a flat list of file paths.
//!   Renames and copies (`R old -> new`, `C old -> new`) emit BOTH paths so
//!   `partition_by_scope` can evaluate scope membership for each side.
//! - `partition_by_scope`: split a dirty-file list into (in_scope,
//!   out_of_scope) with `LOOPR_ARTIFACTS` always filtered to out_of_scope
//!   regardless of the caller's scope list.
//!
//! Ported from v4's `agents/executor/action/scope.rs` with two changes:
//! `LOOPR_ARTIFACTS` is `[".loopr/"]` (v5 puts every orchestrator
//! artifact under one directory); `parse_porcelain_status` emits both
//! sides of an `R`/`C` line instead of only the destination.

const LOOPR_ARTIFACTS: &[&str] = &[".loopr/"];

/// Scope-match semantics (Phase 14). Return `true` when `path` is inside the
/// Work's declared `scope_files`.
///
/// Rules (fail-closed: an unmatched path is out of scope):
/// - A loopr artifact (under `.loopr/`) is ALWAYS out of scope, regardless of
///   `scope_files`.
/// - An empty `scope_files` is the artifact-only fallback: every non-artifact
///   path is in scope. Production Works always carry a non-empty scope (the
///   decomposer rejects an empty one), so this branch only covers legacy /
///   test-constructed Works.
/// - A `scope_files` entry ending in `/` is a directory prefix: `path` matches
///   when it lives under that directory.
/// - Any other entry is an exact repo-relative path: `path` matches on exact
///   equality.
///
/// Both sides have a leading `./` stripped and are compared case-sensitively
/// (Unix convention). The check is against the INTENDED path text, not
/// filesystem existence.
fn is_in_scope(path: &str, scope_files: &[String]) -> bool {
    let normalized = path.strip_prefix("./").unwrap_or(path);
    if LOOPR_ARTIFACTS.iter().any(|a| normalized.starts_with(a)) {
        return false;
    }
    if scope_files.is_empty() {
        return true;
    }
    scope_files.iter().any(|entry| {
        let tag = entry.strip_prefix("./").unwrap_or(entry);
        match tag.strip_suffix('/') {
            // Directory prefix: `path` must be strictly under `dir/`.
            Some(dir) if !dir.is_empty() => {
                let mut prefix = dir.to_string();
                prefix.push('/');
                normalized.starts_with(&prefix)
            }
            // A bare `/` (or empty after stripping) is not a usable directory
            // scope; fail closed rather than match everything.
            Some(_) => false,
            // Exact repo-relative path.
            None => normalized == tag,
        }
    })
}

/// Partition dirty files into in-scope and out-of-scope relative to
/// `scope_files`, using [`is_in_scope`]'s semantics (exact path or trailing-
/// slash directory prefix; `.loopr/` always out-of-scope).
///
/// Emitted paths are normalized: leading `./` is stripped.
pub fn partition_by_scope(dirty_files: &[String], scope_files: &[String]) -> (Vec<String>, Vec<String>) {
    let mut in_scope = Vec::new();
    let mut out_of_scope = Vec::new();
    for file in dirty_files {
        let normalized = file.strip_prefix("./").unwrap_or(file).to_string();
        if is_in_scope(&normalized, scope_files) {
            in_scope.push(normalized);
        } else {
            out_of_scope.push(normalized);
        }
    }
    (in_scope, out_of_scope)
}

/// Return the subset of `paths` that fall OUTSIDE the Work's declared scope,
/// using the same semantics as [`partition_by_scope`]. Used by
/// `propose_bundle` as a defense-in-depth gate: the branch-vs-base diff is
/// re-checked against `scope_files` so any path that reached a commit outside
/// the scoped dispatcher (e.g. a bash `git` action before the denylist, a
/// merge, a stash pop) rejects the propose instead of silently shipping.
///
/// Emitted paths are normalized: leading `./` is stripped.
pub fn out_of_scope_paths(paths: &[String], scope_files: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.strip_prefix("./").unwrap_or(p).to_string())
        .filter(|p| !is_in_scope(p, scope_files))
        .collect()
}

/// Parse `git status --porcelain` output into a flat list of file paths.
/// Handles status prefixes (M, A, D, ??, R, C) and quoted paths.
///
/// Renames (`R  old -> new`) and copies (`C  old -> new`) emit BOTH the
/// old and new path. v4 only emitted the destination, which works under
/// `git add -A` but loses the source-side under explicit
/// `git commit --only -- <paths>`: if scope omits the source path, the
/// rename's deletion side never lands.
pub fn parse_porcelain_status(output: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in output.lines() {
        // Format: "XY filename" or "XY old -> new"
        let Some(rest) = line.get(3..) else { continue };
        let prefix = line.get(..2).unwrap_or("");
        let is_rename_or_copy = prefix.starts_with('R') || prefix.starts_with('C');
        if is_rename_or_copy && rest.contains(" -> ") {
            let mut parts = rest.splitn(2, " -> ");
            if let Some(old) = parts.next() {
                out.push(old.trim_matches('"').to_string());
            }
            if let Some(new) = parts.next() {
                out.push(new.trim_matches('"').to_string());
            }
        } else {
            out.push(rest.trim_matches('"').to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests;
