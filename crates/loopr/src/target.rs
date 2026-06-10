use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::LooprError;

/// Resolve the effective target directory.
///
/// Step 1 (precedence): `-C` flag > `LOOPR_TARGET` env > CWD. Empty env
/// string is treated as unset.
/// Step 2: canonicalize the chosen path. Canonicalize failures or
/// non-directory resolutions map to `LooprError::TargetInvalid`.
/// Step 3 (three-tier root discovery, first match wins):
///   (a) `git -C <path> rev-parse --show-toplevel` -> use that path
///   (b) walk ancestors looking for `.loopr/` -> use first match
///   (c) fall through to the canonicalized start path
///
/// Prior versions also honored a top-level `.taskstore/` marker. Since
/// `store::Store::open` now nests taskstore at `.loopr/taskstore/`, a
/// bare `.taskstore/` at a target's root is not a v5-created directory
/// and no longer counts as an init marker.
pub fn resolve(chdir: Option<&Path>, env: Option<&str>, cwd: &Path) -> Result<PathBuf, LooprError> {
    let canonical = canonical_start(chdir, env, cwd)?;

    if let Some(root) = git_toplevel(&canonical) {
        return Ok(root);
    }
    if let Some(root) = marker_walk(&canonical) {
        return Ok(root);
    }
    Ok(canonical)
}

/// Steps 1-2 of resolution only: pick the start path (`-C` > env > CWD),
/// canonicalize it, and confirm it is a directory. This is the named path
/// the user pointed at, WITHOUT the step-3 walk to a git toplevel / `.loopr`
/// ancestor. `init` compares this against `resolve`'s walked result to refuse
/// silently re-rooting a subdirectory init into the enclosing repo.
pub fn canonical_start(chdir: Option<&Path>, env: Option<&str>, cwd: &Path) -> Result<PathBuf, LooprError> {
    let start = if let Some(p) = chdir {
        p.to_path_buf()
    } else if let Some(v) = env.filter(|s| !s.is_empty()) {
        PathBuf::from(v)
    } else {
        cwd.to_path_buf()
    };

    let canonical = start
        .canonicalize()
        .map_err(|_| LooprError::TargetInvalid { path: start.clone() })?;
    if !canonical.is_dir() {
        return Err(LooprError::TargetIsFile { path: start });
    }
    Ok(canonical)
}

fn git_toplevel(start: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn marker_walk(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        if ancestor.join(".loopr").is_dir() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests;
