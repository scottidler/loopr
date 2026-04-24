use std::path::{Component, Path};

use thiserror::Error;

/// Convert an absolute filesystem path to a claude-style slug.
///
/// Transformation: leading `/` becomes leading `-`; subsequent `/` become
/// `-`. A trailing `/` is stripped. Examples:
///   `/home/saidler/repos/rust-version` -> `-home-saidler-repos-rust-version`
///   `/tmp/a/b/`                        -> `-tmp-a-b`
///
/// The input must be a canonical absolute path. Relative paths, empty
/// paths, and paths containing `..` or `.` components are rejected: the
/// caller should canonicalize first. Collisions are possible in principle
/// (e.g. `/home/a` vs `/home-a` both slug to `-home-a`) but rare with
/// realistic paths; documented as a known edge case.
pub fn target_slug(path: &Path) -> Result<String, TargetSlugError> {
    let s = path
        .to_str()
        .ok_or_else(|| TargetSlugError::NonUtf8(path.to_path_buf().display().to_string()))?;
    if s.is_empty() {
        return Err(TargetSlugError::Empty);
    }
    if !path.is_absolute() {
        return Err(TargetSlugError::NotAbsolute(s.to_string()));
    }
    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::RootDir => out.push('-'),
            Component::Normal(name) => {
                let piece = name.to_str().ok_or_else(|| TargetSlugError::NonUtf8(s.to_string()))?;
                if !out.ends_with('-') {
                    out.push('-');
                }
                out.push_str(piece);
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(TargetSlugError::NonCanonical(s.to_string()));
            }
        }
    }
    Ok(out)
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum TargetSlugError {
    #[error("target path `{0}` is not UTF-8")]
    NonUtf8(String),
    #[error("target path is empty")]
    Empty,
    #[error("target path `{0}` is not absolute")]
    NotAbsolute(String),
    #[error("target path `{0}` is not canonical (contains . or ..)")]
    NonCanonical(String),
}
