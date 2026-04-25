//! Git command wrappers. Every git invocation goes through `git_cmd(repo_path)`
//! which sets `.current_dir(repo_path)` and `.env("LC_ALL", "C")`. The
//! `LC_ALL=C` is mandatory (R2 hardening): without it, localized stderr phrases
//! defeat our `SeqTaken` classifier and a retryable "already exists" error
//! would bubble up as a fatal `GitCommand`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tracing::instrument;

use domain::WorkId;

use crate::error::WorktreeError;

/// Stderr substrings that mean "retry with seq+1" when returned from
/// `git worktree add`. Matched case-sensitively against English phrasing
/// (we force `LC_ALL=C` in `git_cmd`). Git 2.51 on Linux emits:
///
/// - `fatal: '<path>' already exists` (path collision)
/// - `fatal: a branch named '<branch>' already exists` (branch collision)
///
/// The design doc's additional phrases (`"already checked out"`,
/// `"is not an empty directory"`) are historical from older git versions and
/// kept as defensive partial matches.
///
/// We do NOT match on exit code alone: exit 128 is git's generic `fatal:`
/// code for disk-full / permission-denied and retrying on it would spin-loop.
const SEQ_TAKEN_PHRASES: &[&str] = &["already exists", "already checked out", "is not an empty directory"];

/// Outcome of one `git worktree add` attempt.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CreateOutcome {
    Created { path: PathBuf, branch: String },
    SeqTaken,
}

pub(crate) fn git_cmd(repo_path: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_path).env("LC_ALL", "C");
    cmd
}

/// One attempt at creating the worktree for `(work_id, seq)`. Returns
/// `CreateOutcome::SeqTaken` on the retryable "already exists" class; any
/// other non-zero exit surfaces as `WorktreeError::GitCommand`.
#[instrument(
    name = "worktree.ops.try_create_at_seq",
    level = "debug",
    skip_all,
    fields(work_id = %work_id, seq, base_sha = sha),
    err,
)]
pub(crate) fn try_create_at_seq(
    repo_path: &Path,
    worktree_root: &Path,
    work_id: &WorkId,
    seq: u32,
    sha: &str,
) -> Result<CreateOutcome, WorktreeError> {
    let path = worktree_root.join(format!("{}-{}", work_id, seq));
    let branch = format!("loopr/wk-{}-{}", work_id, seq);

    let output = git_cmd(repo_path)
        .args([
            "worktree",
            "add",
            path.to_str().ok_or_else(|| {
                WorktreeError::GitCommand(format!("worktree path not valid UTF-8: {}", path.display()))
            })?,
            "-b",
            &branch,
            sha,
        ])
        .output()?;

    if output.status.success() {
        return Ok(CreateOutcome::Created { path, branch });
    }

    if is_seq_taken(&output) {
        return Ok(CreateOutcome::SeqTaken);
    }

    Err(WorktreeError::GitCommand(format_stderr(&output)))
}

/// Remove a worktree registered at `path`. `git worktree remove --force`
/// handles dirty worktrees; the forcefulness is deliberate because worktree
/// contents are agent-produced output, not user data.
///
/// Idempotent: a path that is not a registered worktree (or has already been
/// removed) returns `Ok(())`.
#[instrument(
    name = "worktree.ops.remove_worktree",
    level = "debug",
    skip_all,
    fields(repo_path = %repo_path.display(), worktree_path = %path.display()),
    err,
)]
pub(crate) fn remove_worktree(repo_path: &Path, path: &Path) -> Result<(), WorktreeError> {
    let output = git_cmd(repo_path)
        .args([
            "worktree",
            "remove",
            "--force",
            path.to_str().ok_or_else(|| {
                WorktreeError::GitCommand(format!("worktree path not valid UTF-8: {}", path.display()))
            })?,
        ])
        .output()?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    // "not a working tree" / "is not a working tree" / "no such path" — all mean
    // the worktree is already gone. Treat as success.
    if stderr.contains("is not a working tree") || stderr.contains("not a valid path") {
        return Ok(());
    }

    Err(WorktreeError::GitCommand(format_stderr(&output)))
}

/// Delete a local branch. `git branch -D <branch>` is force-delete so a
/// branch that has unmerged commits still goes away. Idempotent on missing.
#[instrument(
    name = "worktree.ops.delete_branch",
    level = "debug",
    skip_all,
    fields(repo_path = %repo_path.display(), branch),
    err,
)]
pub(crate) fn delete_branch(repo_path: &Path, branch: &str) -> Result<(), WorktreeError> {
    let output = git_cmd(repo_path).args(["branch", "-D", branch]).output()?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not found") {
        return Ok(());
    }

    Err(WorktreeError::GitCommand(format_stderr(&output)))
}

/// `git worktree prune`. Clears orphaned registrations under
/// `$GIT_DIR/worktrees/` left behind by crashed sessions. Non-fatal: on
/// failure we log a warning and return `Ok` — prune is a best-effort hygiene
/// sweep, not a correctness-critical step.
#[instrument(name = "worktree.ops.prune", level = "debug", skip_all, fields(repo_path = %repo_path.display()), err)]
pub(crate) fn prune(repo_path: &Path) -> Result<(), WorktreeError> {
    let output = git_cmd(repo_path).args(["worktree", "prune"]).output()?;

    if !output.status.success() {
        tracing::warn!(
            repo = %repo_path.display(),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "git worktree prune failed (non-fatal)"
        );
    }
    Ok(())
}

/// Resolve `base_ref` to a 40-char SHA. D10: run in **repo** context, never
/// inside the worktree, because `HEAD` inside a worktree resolves to the
/// worktree's own branch tip rather than the caller's intended base.
#[instrument(
    name = "worktree.ops.resolve_sha",
    level = "debug",
    skip_all,
    fields(repo_path = %repo_path.display(), base_ref),
    err,
)]
pub(crate) fn resolve_sha(repo_path: &Path, base_ref: &str) -> Result<String, WorktreeError> {
    let output = git_cmd(repo_path).args(["rev-parse", base_ref]).output()?;

    if !output.status.success() {
        return Err(WorktreeError::GitCommand(format_stderr(&output)));
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        return Err(WorktreeError::GitCommand(format!(
            "git rev-parse {base_ref}: empty stdout"
        )));
    }
    Ok(sha)
}

/// Run `git branch --show-current` inside `worktree_path`. Used by
/// `Worktree::create` as a post-create defensive check (see v4
/// manager.rs:122-132).
pub(crate) fn show_current_branch(worktree_path: &Path) -> Result<String, WorktreeError> {
    let output = git_cmd(worktree_path).args(["branch", "--show-current"]).output()?;

    if !output.status.success() {
        return Err(WorktreeError::GitCommand(format_stderr(&output)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// List porcelain output of `git worktree list` executed in `repo_path`.
#[instrument(name = "worktree.ops.list_porcelain", level = "debug", skip_all, fields(repo_path = %repo_path.display()), err)]
pub(crate) fn list_porcelain(repo_path: &Path) -> Result<String, WorktreeError> {
    let output = git_cmd(repo_path).args(["worktree", "list", "--porcelain"]).output()?;

    if !output.status.success() {
        return Err(WorktreeError::GitCommand(format_stderr(&output)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn is_seq_taken(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    SEQ_TAKEN_PHRASES.iter().any(|p| stderr.contains(p))
}

fn format_stderr(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".into());
    format!("exit {code}: {}", stderr.trim())
}

#[cfg(test)]
mod tests;
