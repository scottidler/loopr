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
///   (b) walk ancestors looking for `.loopr/` or `.taskstore/` -> use first match
///   (c) fall through to the canonicalized start path
pub fn resolve(chdir: Option<&Path>, env: Option<&str>, cwd: &Path) -> Result<PathBuf, LooprError> {
    let start = if let Some(p) = chdir {
        p.to_path_buf()
    } else if let Some(v) = env {
        PathBuf::from(v)
    } else {
        cwd.to_path_buf()
    };

    let canonical = start
        .canonicalize()
        .map_err(|_| LooprError::TargetInvalid { path: start.clone() })?;
    if !canonical.is_dir() {
        return Err(LooprError::TargetInvalid { path: start });
    }

    if let Some(root) = git_toplevel(&canonical) {
        return Ok(root);
    }
    if let Some(root) = marker_walk(&canonical) {
        return Ok(root);
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
        if ancestor.join(".loopr").is_dir() || ancestor.join(".taskstore").is_dir() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::fs;
    use std::process::Command as PCommand;
    use tempfile::TempDir;

    fn git_init(dir: &Path) {
        let status = PCommand::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
    }

    #[test]
    fn chdir_takes_precedence_over_env_and_cwd() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let c = TempDir::new().unwrap();
        let resolved = resolve(Some(a.path()), Some(b.path().to_str().unwrap()), c.path()).unwrap();
        // No git, no markers; resolution should equal canonicalized `a`.
        assert_eq!(resolved, a.path().canonicalize().unwrap());
    }

    #[test]
    fn env_takes_precedence_over_cwd() {
        let b = TempDir::new().unwrap();
        let c = TempDir::new().unwrap();
        let resolved = resolve(None, Some(b.path().to_str().unwrap()), c.path()).unwrap();
        assert_eq!(resolved, b.path().canonicalize().unwrap());
    }

    #[test]
    fn cwd_used_when_chdir_and_env_unset() {
        let c = TempDir::new().unwrap();
        let resolved = resolve(None, None, c.path()).unwrap();
        assert_eq!(resolved, c.path().canonicalize().unwrap());
    }

    #[test]
    fn invalid_path_errors() {
        let fake = PathBuf::from("/does/not/exist/anywhere/42");
        let err = resolve(Some(&fake), None, Path::new("/tmp")).unwrap_err();
        assert!(matches!(err, LooprError::TargetInvalid { .. }));
    }

    #[test]
    fn file_path_errors_as_target_invalid() {
        let td = TempDir::new().unwrap();
        let file = td.path().join("a-file");
        fs::write(&file, "").unwrap();
        let err = resolve(Some(&file), None, Path::new("/tmp")).unwrap_err();
        assert!(matches!(err, LooprError::TargetInvalid { .. }));
    }

    #[test]
    fn git_repo_resolves_to_toplevel() {
        let td = TempDir::new().unwrap();
        git_init(td.path());
        let sub = td.path().join("src/foo/bar");
        fs::create_dir_all(&sub).unwrap();
        let resolved = resolve(Some(&sub), None, Path::new("/tmp")).unwrap();
        assert_eq!(resolved, td.path().canonicalize().unwrap());
    }

    #[test]
    fn marker_walk_finds_loopr_dir() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join(".loopr")).unwrap();
        let sub = td.path().join("src/foo");
        fs::create_dir_all(&sub).unwrap();
        // No git init, so tier (a) fails; tier (b) should catch it.
        let resolved = resolve(Some(&sub), None, Path::new("/tmp")).unwrap();
        assert_eq!(resolved, td.path().canonicalize().unwrap());
    }

    #[test]
    fn marker_walk_finds_taskstore_dir() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join(".taskstore")).unwrap();
        let sub = td.path().join("src/foo");
        fs::create_dir_all(&sub).unwrap();
        let resolved = resolve(Some(&sub), None, Path::new("/tmp")).unwrap();
        assert_eq!(resolved, td.path().canonicalize().unwrap());
    }

    #[test]
    fn fall_through_to_start_when_nothing_found() {
        let td = TempDir::new().unwrap();
        // No git, no markers. Should resolve to td itself.
        let resolved = resolve(Some(td.path()), None, Path::new("/tmp")).unwrap();
        assert_eq!(resolved, td.path().canonicalize().unwrap());
    }
}
