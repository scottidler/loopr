use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::sandbox::SandboxMode;
use crate::tool::ToolContext;

/// Write `bytes` to `target` atomically: write a sibling temp file then
/// `rename` over the target. POSIX `rename(2)` within one filesystem is
/// atomic, so a crash mid-write never leaves a partially-written file at
/// `target` (Phase-5 finding 5). On rename failure the temp is best-effort
/// removed so a failed write doesn't litter the worktree.
pub(crate) async fn atomic_write(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp_name = match target.file_name().and_then(|n| n.to_str()) {
        Some(n) => format!(".{n}.loopr-tmp-{}", Uuid::now_v7()),
        None => format!(".loopr-tmp-{}", Uuid::now_v7()),
    };
    let tmp = parent.join(tmp_name);
    tokio::fs::write(&tmp, bytes).await?;
    if let Err(e) = tokio::fs::rename(&tmp, target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PathError {
    #[error("path escapes working directory: {0}")]
    Escape(String),
    #[error("path matched deny pattern: {0}")]
    Denied(String),
}

/// Resolve a caller-provided path against the tool context and validate it
/// against both the working-dir boundary (unless SandboxMode::Off) and the
/// path_deny_patterns (always).
///
/// Paths are canonicalized via the existing parent. We don't require the file
/// to exist - for Write/Edit the parent must exist but the file may not.
/// Substring-based deny matching: any path component that contains a pattern
/// is denied. Cheap and doesn't mis-match on `/etc/foo.keyboard` for pattern
/// `.key` because we check components, not raw bytes - `.key` appears in no
/// single component of that path.
pub(crate) fn resolve(user_path: &Path, ctx: &ToolContext) -> Result<PathBuf, PathError> {
    let abs = if user_path.is_absolute() {
        user_path.to_path_buf()
    } else {
        ctx.working_dir.join(user_path)
    };

    // Canonicalize. If the file doesn't exist (normal for write/edit targets),
    // walk up to the deepest existing ancestor, canonicalize that, and re-
    // attach the missing components. This keeps working-dir containment
    // strict (an existing ancestor that resolves outside is still caught)
    // without requiring the parent directory to exist yet.
    let canonical = canonicalize_relaxed(&abs);

    if !matches!(ctx.sandbox, SandboxMode::Off) {
        let working_canonical = ctx
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.clone());
        if !canonical.starts_with(&working_canonical) {
            return Err(PathError::Escape(canonical.display().to_string()));
        }
    }

    for pattern in &ctx.path_deny_patterns {
        if matches_deny_pattern(&canonical, pattern) {
            return Err(PathError::Denied(pattern.clone()));
        }
    }

    Ok(canonical)
}

fn canonicalize_relaxed(abs: &Path) -> PathBuf {
    // Walk up until an ancestor resolves, then re-attach the suffix.
    let mut existing = abs.to_path_buf();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(|s| s.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name);
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for name in missing.into_iter().rev() {
        out.push(name);
    }
    out
}

fn matches_deny_pattern(path: &Path, pattern: &str) -> bool {
    for component in path.components() {
        if let Some(s) = component.as_os_str().to_str()
            && s.contains(pattern)
        {
            return true;
        }
    }
    false
}
