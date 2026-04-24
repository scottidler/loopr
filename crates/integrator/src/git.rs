//! Private git subprocess helpers used by `integrate`.
//!
//! Every helper wraps `tokio::process::Command` with `git -C <target>`
//! and enforces `IntegratorConfig::git_timeout`. On non-zero exit the
//! helper returns a typed `IntegrationError` rather than a bare status
//! code; the pattern mirrors `agents::reviewer::git_show`.
//!
//! Arguments come from typed fields (plan ids, bundle branch names,
//! SHAs); no user-supplied strings reach subprocess args, and
//! `Command::arg()` is used per-argument so `sh -c` interpolation
//! hazards do not apply.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::error::IntegrationError;

/// Verify a ref exists via `git rev-parse --verify <ref>`. Returns
/// `Ok(true)` if the ref resolves, `Ok(false)` if git exits non-zero
/// (ref does not exist), `Err(Git)` on subprocess failure or timeout.
pub(crate) async fn verify_branch(
    target: &Path,
    branch: &str,
    git_timeout: Duration,
) -> Result<bool, IntegrationError> {
    let out = run_git(target, &["rev-parse", "--verify", branch], git_timeout).await?;
    Ok(out.status.success())
}

/// Check out a branch via `git checkout <branch>`. Fatal on failure
/// (a dirty working tree, a missing branch, or a permission error
/// all surface here).
pub(crate) async fn checkout(target: &Path, branch: &str, git_timeout: Duration) -> Result<(), IntegrationError> {
    let out = run_git(target, &["checkout", branch], git_timeout).await?;
    if !out.status.success() {
        return Err(IntegrationError::Git(format!(
            "git checkout {branch} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Return the current `HEAD` SHA via `git rev-parse HEAD`.
pub(crate) async fn rev_parse_head(target: &Path, git_timeout: Duration) -> Result<String, IntegrationError> {
    let out = run_git(target, &["rev-parse", "HEAD"], git_timeout).await?;
    if !out.status.success() {
        return Err(IntegrationError::Git(format!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Check that `bundle_branch` has commits beyond the merge base with
/// `HEAD`. Returns `Err(EmptyBranch)` when `merge-base HEAD <branch>`
/// equals `rev-parse <branch>` - in that case `git merge --no-ff`
/// would exit 0 ("Already up to date") with no merge commit.
///
/// Safe to call ONLY when the branch has NOT already been merged into
/// HEAD. If the branch is already merged, merge-base HEAD <branch> ==
/// <branch>'s head, which would falsely trip EmptyBranch. Use the
/// `is_ancestor` check first on crash-recovery (`Integrating`)
/// bundles to rule out the already-merged case before calling this.
pub(crate) async fn assert_nontrivial_branch(
    target: &Path,
    bundle_id: &str,
    branch: &str,
    git_timeout: Duration,
) -> Result<(), IntegrationError> {
    let base = run_git(target, &["merge-base", "HEAD", branch], git_timeout).await?;
    let head = run_git(target, &["rev-parse", branch], git_timeout).await?;
    if !base.status.success() {
        return Err(IntegrationError::Git(format!(
            "git merge-base HEAD {branch} failed: {}",
            String::from_utf8_lossy(&base.stderr)
        )));
    }
    if !head.status.success() {
        return Err(IntegrationError::Git(format!(
            "git rev-parse {branch} failed: {}",
            String::from_utf8_lossy(&head.stderr)
        )));
    }
    let base_resolved = String::from_utf8_lossy(&base.stdout).trim().to_string();
    let head_resolved = String::from_utf8_lossy(&head.stdout).trim().to_string();
    if base_resolved == head_resolved {
        return Err(IntegrationError::EmptyBranch {
            bundle_id: bundle_id.to_string(),
            branch: branch.to_string(),
        });
    }
    Ok(())
}

/// Ancestry check via `git merge-base --is-ancestor <commit> <ref>`.
/// Exit 0 = `commit` is an ancestor of `ref`, exit 1 = not an
/// ancestor, any other exit is a git failure.
///
/// Used by the crash-recovery idempotency check: if a prior
/// `integrate` call merged the Bundle's `head_commit` before crashing,
/// that commit will be an ancestor of the integration branch's
/// current `HEAD`.
pub(crate) async fn is_ancestor(
    target: &Path,
    commit: &str,
    ref_name: &str,
    git_timeout: Duration,
) -> Result<bool, IntegrationError> {
    let out = run_git(target, &["merge-base", "--is-ancestor", commit, ref_name], git_timeout).await?;
    if let Some(code) = out.status.code() {
        match code {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(IntegrationError::Git(format!(
                "git merge-base --is-ancestor {commit} {ref_name} exited {code}: {}",
                String::from_utf8_lossy(&out.stderr)
            ))),
        }
    } else {
        Err(IntegrationError::Git(format!(
            "git merge-base --is-ancestor {commit} {ref_name} terminated by signal"
        )))
    }
}

/// Resolve the chronologically **first** merge commit in the ancestry
/// path from `bundle_head` to `HEAD`, i.e., the merge commit that
/// actually absorbed `bundle_head`.
///
/// The `--reverse` is load-bearing: git's default order is
/// reverse-chronological, so piping that to `head -n1` would yield
/// the newest merge (whichever Bundle was integrated most recently),
/// not the merge that absorbed the requested `bundle_head`.
pub(crate) async fn merge_commit_sha_for(
    target: &Path,
    bundle_head: &str,
    git_timeout: Duration,
) -> Result<String, IntegrationError> {
    let range = format!("{bundle_head}..HEAD");
    let out = run_git(
        target,
        &["log", "--merges", "--format=%H", "--ancestry-path", "--reverse", &range],
        git_timeout,
    )
    .await?;
    if !out.status.success() {
        return Err(IntegrationError::Git(format!(
            "git log --ancestry-path {bundle_head}..HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    match stdout.lines().next() {
        Some(sha) => Ok(sha.trim().to_string()),
        None => Err(IntegrationError::Git(format!(
            "no merge commit found in ancestry path {bundle_head}..HEAD"
        ))),
    }
}

/// Attempt `git merge --no-ff <branch> -m "Merge bundle branch <branch>"`.
/// On success returns the new `HEAD` SHA (via a follow-up `rev-parse HEAD`).
/// On non-zero exit returns the stderr for the caller to classify; the
/// caller is responsible for running `merge_abort` + `reset_hard`.
pub(crate) async fn merge_no_ff(
    target: &Path,
    branch: &str,
    git_timeout: Duration,
) -> Result<Result<String, String>, IntegrationError> {
    let message = format!("Merge bundle branch {branch}");
    let out = run_git(target, &["merge", "--no-ff", branch, "-m", &message], git_timeout).await?;
    if !out.status.success() {
        return Ok(Err(String::from_utf8_lossy(&out.stderr).to_string()));
    }
    let sha = rev_parse_head(target, git_timeout).await?;
    Ok(Ok(sha))
}

/// Best-effort `git merge --abort`. Errors are swallowed because the
/// merge may not have reached conflict state (e.g., a subprocess
/// failure earlier in the sequence would leave no merge to abort).
pub(crate) async fn merge_abort(target: &Path, git_timeout: Duration) {
    let _ = run_git(target, &["merge", "--abort"], git_timeout).await;
}

/// Reset the current branch hard to the given SHA. A failure here is
/// fatal: rollback could not restore the integration branch, and the
/// daemon's worktree-crash-recovery pass at restart owns the repair.
pub(crate) async fn reset_hard(target: &Path, sha: &str, git_timeout: Duration) -> Result<(), IntegrationError> {
    let out = run_git(target, &["reset", "--hard", sha], git_timeout).await?;
    if !out.status.success() {
        return Err(IntegrationError::Git(format!(
            "git reset --hard {sha} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Private subprocess wrapper
// ---------------------------------------------------------------------------

struct CapturedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_git(target: &Path, args: &[&str], git_timeout: Duration) -> Result<CapturedOutput, IntegrationError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(target);
    for arg in args {
        cmd.arg(arg);
    }
    let fut = cmd.output();
    let out = match timeout(git_timeout, fut).await {
        Ok(Ok(out)) => out,
        Ok(Err(io)) => return Err(IntegrationError::Io(io)),
        Err(_) => {
            return Err(IntegrationError::Git(format!(
                "git {:?} timed out after {:?}",
                args, git_timeout
            )));
        }
    };
    Ok(CapturedOutput {
        status: out.status,
        stdout: out.stdout,
        stderr: out.stderr,
    })
}
