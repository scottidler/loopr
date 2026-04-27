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

/// Partition dirty files into in-scope and out-of-scope relative to `scope_files`.
///
/// A file is in-scope if:
/// - It is NOT a loopr artifact (always filtered, regardless of `scope_files`)
/// - AND it matches at least one entry in `scope_files` as an exact path
/// - OR `scope_files` is empty (artifact-only filtering)
///
/// Both sides are normalized: leading `./` is stripped, paths are compared
/// case-sensitively (Unix convention).
pub fn partition_by_scope(dirty_files: &[String], scope_files: &[String]) -> (Vec<String>, Vec<String>) {
    let mut in_scope = Vec::new();
    let mut out_of_scope = Vec::new();
    for file in dirty_files {
        let normalized = file.strip_prefix("./").unwrap_or(file);
        let is_artifact = LOOPR_ARTIFACTS.iter().any(|a| normalized.starts_with(a));
        if is_artifact {
            out_of_scope.push(normalized.to_string());
            continue;
        }
        // Empty scope = artifact-only filter: every non-artifact path is in-scope.
        let matches_tag = scope_files.is_empty()
            || scope_files
                .iter()
                .any(|tag| normalized == tag.strip_prefix("./").unwrap_or(tag));
        if matches_tag {
            in_scope.push(normalized.to_string());
        } else {
            out_of_scope.push(normalized.to_string());
        }
    }
    (in_scope, out_of_scope)
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
