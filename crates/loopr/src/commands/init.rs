//! `loopr init` body.
//!
//! Phase 9 of the Tier-1 cleanup expanded init from "seed prompts"
//! into the six-step orchestrator the design doc had always
//! described:
//!
//! 1. `verify_source_guard(target)` — re-confirm we are not pointing
//!    at a loopr source tree.
//! 2. `create_loopr_dir(target)` — mkdir `<target>/.loopr/`.
//! 3. `open_taskstore(target)` — `Store::open` materializes
//!    `<target>/.loopr/taskstore/` on first call.
//! 4. `install_taskstore_hooks(target)` — `Store::install_git_hooks`
//!    in-process. Idempotent; already-installed hooks preserved.
//! 5. `ensure_git_excludes(target)` — `worktree::ensure_loopr_excludes`.
//! 6. `seed_prompts(target, force)` — existing seeder, unchanged.
//!
//! Each step returns a `StepOutcome` and the final report prints one
//! human-readable line per step plus a totals line. Idempotent: a
//! re-run on a fully-initialized target prints `preserved` lines and
//! exits 0.

use std::path::{Path, PathBuf};

use include_dir::{Dir, DirEntry};

use crate::error::LooprError;

/// Per-step result. The detail string is rendered into the output
/// line; `Skipped { reason }` is reserved for steps that intentionally
/// did nothing (e.g. a non-git target where `install_git_hooks` has
/// nothing to attach to).
#[derive(Debug)]
pub enum StepOutcome {
    Created { detail: String },
    Preserved { detail: String },
    Skipped { reason: String },
}

/// Outcome summary printed at the end of `loopr init`. Carries one
/// `StepOutcome` per step plus a tally of seed-prompts results.
#[derive(Debug, Default)]
pub struct InitOutcome {
    pub written: usize,
    pub preserved: usize,
}

#[tracing::instrument(name = "client.init", level = "info", skip_all, fields(target = %target.display(), force), err)]
pub fn run(target: &Path, force: bool) -> Result<(), LooprError> {
    // The four new steps need an async runtime for the taskstore
    // calls; the existing seed_prompts is sync and runs after.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::DaemonStartup(format!("init runtime build: {e}")))?;

    let report = rt.block_on(async {
        let guard = step_verify_source_guard(target)?;
        let loopr_dir = step_create_loopr_dir(target)?;
        let taskstore = step_open_taskstore(target).await?;
        let hooks = step_install_taskstore_hooks(target).await?;
        let excludes = step_ensure_git_excludes(target)?;
        let prompts = step_seed_prompts(target, force)?;
        Ok::<_, LooprError>([guard, loopr_dir, taskstore, hooks, excludes, prompts])
    })?;

    for (label, outcome) in [
        "source-guard",
        "loopr-dir",
        "taskstore",
        "git-hooks",
        "git-excludes",
        "prompts",
    ]
    .iter()
    .zip(report.iter())
    {
        match outcome {
            StepOutcome::Created { detail } => println!("[created] {label}: {detail}"),
            StepOutcome::Preserved { detail } => println!("[preserved] {label}: {detail}"),
            StepOutcome::Skipped { reason } => println!("[skipped] {label}: {reason}"),
        }
    }
    let created = report
        .iter()
        .filter(|o| matches!(o, StepOutcome::Created { .. }))
        .count();
    let preserved = report
        .iter()
        .filter(|o| matches!(o, StepOutcome::Preserved { .. }))
        .count();
    println!(
        "init complete: {created} created, {preserved} preserved at {}",
        target.display()
    );
    Ok(())
}

fn step_verify_source_guard(target: &Path) -> Result<StepOutcome, LooprError> {
    crate::guard::check(target)?;
    // The guard checks; it creates nothing. Report Preserved so a re-run's
    // output doesn't claim a fresh creation that never happened.
    Ok(StepOutcome::Preserved {
        detail: "source-guard passed".to_string(),
    })
}

fn step_create_loopr_dir(target: &Path) -> Result<StepOutcome, LooprError> {
    let dir = target.join(".loopr");
    let preexisting = dir.try_exists().unwrap_or(false);
    std::fs::create_dir_all(&dir).map_err(|e| LooprError::DaemonStartup(format!("mkdir {dir:?}: {e}")))?;
    if preexisting {
        Ok(StepOutcome::Preserved {
            detail: format!(".loopr/ already at {}", dir.display()),
        })
    } else {
        Ok(StepOutcome::Created {
            detail: format!(".loopr/ at {}", dir.display()),
        })
    }
}

async fn step_open_taskstore(target: &Path) -> Result<StepOutcome, LooprError> {
    let taskstore_dir = target.join(store::TASKSTORE_SUBPATH);
    let preexisting = taskstore_dir.try_exists().unwrap_or(false);
    let store = store::Store::open(target)
        .await
        .map_err(|e| LooprError::DaemonStartup(format!("Store::open: {e}")))?;
    // Best-effort close; init's purpose is to materialize the
    // directory, not to keep the Store open. Failure here is non-fatal.
    if let Err(e) = store.close().await {
        tracing::warn!(error = %e, "Store::close after init open failed (non-fatal)");
    }
    if preexisting {
        Ok(StepOutcome::Preserved {
            detail: format!(".loopr/taskstore/ already initialized at {}", taskstore_dir.display()),
        })
    } else {
        Ok(StepOutcome::Created {
            detail: format!(".loopr/taskstore/ initialized at {}", taskstore_dir.display()),
        })
    }
}

async fn step_install_taskstore_hooks(target: &Path) -> Result<StepOutcome, LooprError> {
    let git_dir = target.join(".git");
    if !git_dir.is_dir() {
        // Non-git target: hooks have nothing to attach to. The
        // source-guard already accepts non-git targets for testing
        // purposes, so we report Skipped rather than failing.
        return Ok(StepOutcome::Skipped {
            reason: "target is not a git repository; nothing to install hooks against".to_string(),
        });
    }

    // Detect installation by a taskstore CONTENT marker, not mere filename
    // existence: a husky/user `pre-commit` file would otherwise read as
    // "taskstore installed" and the merge driver — the cultural mitigation
    // for taskstore's documented JSONL-corruption mode — would silently
    // never get installed. The installer (`install_git_hooks`) is itself
    // idempotent and content-aware (taskstore's `install_hook` appends its
    // `taskstore sync` line only when absent), so we ALWAYS run it and use
    // the pre-existing marker only to decide the Created-vs-Preserved label.
    let hooks_dir = git_dir.join("hooks");
    let was_present = hook_has_taskstore_marker(&hooks_dir.join("pre-commit"));

    let store = store::Store::open(target)
        .await
        .map_err(|e| LooprError::DaemonStartup(format!("Store::open for hooks: {e}")))?;
    store
        .install_git_hooks()
        .await
        .map_err(|e| LooprError::DaemonStartup(format!("install_git_hooks: {e}")))?;
    if let Err(e) = store.close().await {
        tracing::warn!(error = %e, "Store::close after install_git_hooks failed (non-fatal)");
    }
    if was_present {
        Ok(StepOutcome::Preserved {
            detail: format!(
                "taskstore hooks + merge driver already installed at {}",
                hooks_dir.display()
            ),
        })
    } else {
        Ok(StepOutcome::Created {
            detail: format!("taskstore hooks + merge driver installed at {}", hooks_dir.display()),
        })
    }
}

/// The content marker taskstore writes into every hook it manages
/// (`# Auto-generated by taskstore\ntaskstore sync\n`). Filename existence
/// alone is insufficient — only this marker distinguishes a taskstore hook
/// from a husky/user hook of the same name.
const TASKSTORE_HOOK_MARKER: &str = "taskstore sync";

fn hook_has_taskstore_marker(hook_path: &Path) -> bool {
    std::fs::read_to_string(hook_path)
        .map(|s| s.contains(TASKSTORE_HOOK_MARKER))
        .unwrap_or(false)
}

fn step_ensure_git_excludes(target: &Path) -> Result<StepOutcome, LooprError> {
    // Mirror the hooks step's git check: on a non-git target, fabricating
    // `.git/info/exclude` would create a phantom `.git/` directory that a
    // SUBSEQUENT init run then mistakes for a real repo and tries to attach
    // hooks against. There is nothing to exclude without a real `.git/`.
    let git_dir = target.join(".git");
    if !git_dir.is_dir() {
        return Ok(StepOutcome::Skipped {
            reason: "target is not a git repository; no .git/info/exclude to manage".to_string(),
        });
    }
    let exclude_path = git_dir.join("info").join("exclude");
    let before_lines = read_line_count(&exclude_path);
    worktree::ensure_loopr_excludes(target)
        .map_err(|e| LooprError::DaemonStartup(format!("ensure_loopr_excludes: {e}")))?;
    let after_lines = read_line_count(&exclude_path);
    if after_lines > before_lines {
        Ok(StepOutcome::Created {
            detail: format!("{} new lines in {}", after_lines - before_lines, exclude_path.display()),
        })
    } else {
        Ok(StepOutcome::Preserved {
            detail: ".git/info/exclude already up to date".to_string(),
        })
    }
}

fn step_seed_prompts(target: &Path, force: bool) -> Result<StepOutcome, LooprError> {
    let prompts_dir = target.join(".loopr").join("prompts");
    let baked = ::context::baked_prompts();
    let outcome = seed_prompts(&prompts_dir, baked, force)?;
    if outcome.written == 0 && outcome.preserved > 0 {
        Ok(StepOutcome::Preserved {
            detail: format!(
                "{} prompt files preserved at {}",
                outcome.preserved,
                prompts_dir.display()
            ),
        })
    } else {
        Ok(StepOutcome::Created {
            detail: format!(
                "{} written, {} preserved at {}",
                outcome.written,
                outcome.preserved,
                prompts_dir.display()
            ),
        })
    }
}

fn read_line_count(path: &Path) -> usize {
    std::fs::read_to_string(path).map(|s| s.lines().count()).unwrap_or(0)
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
