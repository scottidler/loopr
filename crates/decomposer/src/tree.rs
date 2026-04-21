//! Workspace file-tree collection for prompt grounding.
//!
//! The decomposer injects the target repo's file tree into the system
//! prompt so the LLM can name real files instead of hallucinating a
//! shape. Two paths:
//!
//! 1. **`git ls-files --cached --others --exclude-standard`** (primary):
//!    tracked + untracked-not-ignored files, honoring `.gitignore`.
//!    This is the cheap, correct path for any target that is a git
//!    repo — which is most of them.
//!
//! 2. **`std::fs` depth-limited walk** (fallback): used when `git` is
//!    not installed or exits non-zero (not a git repo, corrupt repo,
//!    etc.). Does NOT honor `.gitignore` — the Stage 7 `context-builder`
//!    crate earns the proper ignore-walker. Instead, a hardcoded
//!    skip-list covers the common noise: `.git/`, `target/`,
//!    `node_modules/`, `.venv/`, `dist/`, `build/`, plus any
//!    dot-prefixed directory.
//!
//! Entry cap at `MAX_ENTRIES` (500); anything beyond is truncated with
//! a `... and N more entries` marker so the LLM knows it isn't seeing
//! the complete tree.

// Phase 2 ships the tree collector ahead of Phase 4's `decompose`
// function, which is its sole caller. The allow is lifted in Phase 4
// once `decompose.rs` wires in `collect_workspace_tree`. Per
// `rust.md`, dead-code allows are tolerated only during active
// transitions; this is one.
#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::error::DecomposerError;

#[cfg(test)]
mod tests;

/// Hard cap on tree entries emitted to the prompt. Enough for the LLM
/// to see workspace shape for decomposition; more than that and the
/// token spend on the tree exceeds the prompt's usefulness. Bump if
/// Stage 7 data shows decomposition quality suffers on larger
/// workspaces.
const MAX_ENTRIES: usize = 500;

/// Maximum recursion depth for the `std::fs` fallback walk.
const FALLBACK_MAX_DEPTH: usize = 4;

/// Hardcoded skip-list for the non-git fallback. Not exhaustive and
/// does not respect `.gitignore`; Stage 7's `context-builder` will do
/// that properly.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "dist", "build"];

/// Collect a newline-separated listing of files in `target`.
///
/// Returns a string suitable for interpolation into the system prompt.
/// Empty workspace yields the sentinel string `"(empty workspace)"`
/// (a single line) so the prompt always has something concrete.
///
/// `target` must exist and be a directory. Missing / non-directory
/// targets surface as `DecomposerError::WorkspaceScanFailed`.
pub(crate) fn collect_workspace_tree(target: &Path) -> Result<String, DecomposerError> {
    if !target.exists() {
        return Err(DecomposerError::WorkspaceScanFailed(format!(
            "target does not exist: {}",
            target.display()
        )));
    }
    if !target.is_dir() {
        return Err(DecomposerError::WorkspaceScanFailed(format!(
            "target is not a directory: {}",
            target.display()
        )));
    }

    let entries = match collect_via_git(target) {
        Ok(entries) => entries,
        Err(_) => collect_via_walk(target)?,
    };

    Ok(format_entries(entries))
}

/// Primary path: `git ls-files -z --cached --others --exclude-standard`
/// in `target`. `-z` delimits entries with NUL so filenames with
/// unusual bytes (non-ASCII, newlines) split correctly. Sort for
/// determinism because git's ordering is not guaranteed stable across
/// versions.
///
/// Returns `Err` with a description when:
/// - the `git` binary is not found on PATH
/// - `git ls-files` exits non-zero (not a repo, corrupt repo, etc.)
///
/// Callers fall through to the `std::fs` walk on any `Err`.
fn collect_via_git(target: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .current_dir(target)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git ls-files exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let mut entries: Vec<String> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|slice| !slice.is_empty())
        .map(|slice| String::from_utf8_lossy(slice).into_owned())
        .collect();
    entries.sort();
    Ok(entries)
}

/// Fallback walk used when `git ls-files` is unavailable or errors.
/// Depth-limited to `FALLBACK_MAX_DEPTH`; skips `SKIP_DIRS` and any
/// dot-prefixed directory. File entries are emitted as paths relative
/// to `target` using forward slashes regardless of platform (matches
/// `git ls-files`'s output shape).
fn collect_via_walk(target: &Path) -> Result<Vec<String>, DecomposerError> {
    let mut out = Vec::new();
    walk(target, target, 0, &mut out).map_err(|e| DecomposerError::WorkspaceScanFailed(e.to_string()))?;
    out.sort();
    Ok(out)
}

fn walk(root: &Path, current: &Path, depth: usize, out: &mut Vec<String>) -> std::io::Result<()> {
    if depth > FALLBACK_MAX_DEPTH {
        return Ok(());
    }
    let entries = fs::read_dir(current)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if name_str.starts_with('.') || SKIP_DIRS.iter().any(|skip| *skip == name_str.as_ref()) {
                continue;
            }
            walk(root, &path, depth + 1, out)?;
        } else if file_type.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Format the collected entries, applying the `MAX_ENTRIES` cap and
/// the empty-workspace sentinel.
fn format_entries(entries: Vec<String>) -> String {
    if entries.is_empty() {
        return "(empty workspace)".to_string();
    }
    let total = entries.len();
    let mut lines: Vec<String> = entries.into_iter().take(MAX_ENTRIES).collect();
    if total > MAX_ENTRIES {
        lines.push(format!("... and {} more entries", total - MAX_ENTRIES));
    }
    lines.join("\n")
}
