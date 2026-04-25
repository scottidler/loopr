//! `loopr init` body. Seeds `<target>/.loopr/prompts/` from the
//! baked `.pmt` tree (embedded in the binary via `include_dir!()` at
//! the `context` crate). Idempotent merge by default: existing files
//! are preserved; missing files are written. `--force` overwrites.
//!
//! `.gitkeep` files in the baked tree are placeholders for git only;
//! the seeder skips writing them but still creates their parent
//! directories so the user sees the empty-slot layout for unbuilt
//! agents and decomposer tiers.
//!
//! Defense-in-depth: every computed destination path is asserted to
//! be a descendant of `<target>/.loopr/prompts/` before writing. The
//! baked tree's relative paths contain no `..` components by
//! construction (we control the source files), but the assertion
//! documents the invariant.

use std::path::{Path, PathBuf};

use include_dir::{Dir, DirEntry};

use crate::error::LooprError;

/// Outcome summary printed at the end of `loopr init`.
#[derive(Debug, Default)]
pub struct InitOutcome {
    pub written: usize,
    pub preserved: usize,
}

#[tracing::instrument(name = "client.init", level = "info", skip_all, fields(target = %target.display(), force), err)]
pub fn run(target: &Path, force: bool) -> Result<(), LooprError> {
    let prompts_dir = target.join(".loopr").join("prompts");
    let baked = ::context::baked_prompts();
    let outcome = seed_prompts(&prompts_dir, baked, force)?;
    println!(
        "Wrote {} files, preserved {} existing files in {}",
        outcome.written,
        outcome.preserved,
        prompts_dir.display()
    );
    Ok(())
}

/// Walk the baked tree and write each `.pmt` file under `<dest>`.
/// Skips `.gitkeep` placeholders but still ensures their parent
/// directory exists. Default mode preserves any pre-existing file at
/// the destination; `force` overwrites.
pub fn seed_prompts(dest: &Path, baked: &Dir<'_>, force: bool) -> Result<InitOutcome, LooprError> {
    let mut outcome = InitOutcome::default();
    seed_dir(dest, dest, baked, force, &mut outcome)?;
    Ok(outcome)
}

fn seed_dir(
    seed_root: &Path,
    dest: &Path,
    dir: &Dir<'_>,
    force: bool,
    outcome: &mut InitOutcome,
) -> Result<(), LooprError> {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(d) => {
                let sub_dest = dest_for(seed_root, dest, d.path())?;
                std::fs::create_dir_all(&sub_dest)
                    .map_err(|e| LooprError::DaemonStartup(format!("mkdir {sub_dest:?}: {e}")))?;
                seed_dir(seed_root, dest, d, force, outcome)?;
            }
            DirEntry::File(f) => {
                let path = f.path();
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let sub_dest = dest_for(seed_root, dest, path)?;
                if let Some(parent) = sub_dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| LooprError::DaemonStartup(format!("mkdir {parent:?}: {e}")))?;
                }
                if file_name == ".gitkeep" {
                    // Parent dir already created above; nothing to write.
                    continue;
                }
                if sub_dest.exists() && !force {
                    outcome.preserved += 1;
                    continue;
                }
                std::fs::write(&sub_dest, f.contents())
                    .map_err(|e| LooprError::DaemonStartup(format!("write {sub_dest:?}: {e}")))?;
                outcome.written += 1;
            }
        }
    }
    Ok(())
}

/// Compute the destination path for a baked tree entry, asserting it
/// stays under `seed_root`. Returns `LooprError::DaemonStartup` if a
/// path traversal would escape the seed root (defense-in-depth; the
/// baked tree's paths cannot contain `..` by construction).
fn dest_for(seed_root: &Path, dest: &Path, baked_rel: &Path) -> Result<PathBuf, LooprError> {
    let candidate = dest.join(baked_rel);
    if !candidate.starts_with(seed_root) {
        return Err(LooprError::DaemonStartup(format!(
            "init refused to write outside seed root: candidate={candidate:?}, seed_root={seed_root:?}"
        )));
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests;
