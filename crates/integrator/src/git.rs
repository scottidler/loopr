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
use tracing::{debug, instrument};

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
#[instrument(name = "integrator.git.checkout", level = "debug", skip_all, fields(branch = branch), err)]
pub(crate) async fn checkout(target: &Path, branch: &str, git_timeout: Duration) -> Result<(), IntegrationError> {
    let out = run_git(target, &["checkout", branch], git_timeout).await?;
    if !out.status.success() {
        return Err(IntegrationError::Git(format!(
            "git checkout {branch} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    debug!(branch, "integrator.git: checkout ok");
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

/// Return the current branch name via `git rev-parse --abbrev-ref HEAD`.
/// Used by the no-branch override (Phase C) to integrate onto the
/// checked-out branch instead of a per-Plan `loopr/plan-<id>` branch.
pub(crate) async fn current_branch(target: &Path, git_timeout: Duration) -> Result<String, IntegrationError> {
    let out = run_git(target, &["rev-parse", "--abbrev-ref", "HEAD"], git_timeout).await?;
    if !out.status.success() {
        return Err(IntegrationError::Git(format!(
            "git rev-parse --abbrev-ref HEAD failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Return `true` if the working tree has any uncommitted OPERATOR state
/// (tracked modifications, staged changes, or untracked files) via
/// `git status --porcelain`. The no-branch override uses this to refuse
/// integrating on top of a dirty tree: `git checkout <current-branch>` is
/// a no-op that does NOT fail on a dirty tree (unlike checking out a
/// different per-Plan branch), so without this guard the Integrator would
/// `git merge` on top of the operator's uncommitted work.
///
/// `.loopr/` is excluded from the check via a git pathspec: it is loopr's
/// OWN run-local state, not the operator's work. Most of it is already
/// covered by `worktree::ensure_loopr_excludes`, but `.loopr/taskstore/`
/// is deliberately NOT excluded there (per vision, TaskStore is committed)
/// — so a target whose taskstore is untracked (test harnesses) or merely
/// dirty between the taskstore git-hook commits (production) reports
/// `?? .loopr/` / ` M .loopr/taskstore/...` and would otherwise trip this
/// guard on loopr's own bookkeeping rather than genuine operator work. The
/// pathspec keeps the guard protecting only operator changes.
pub(crate) async fn working_tree_dirty(target: &Path, git_timeout: Duration) -> Result<bool, IntegrationError> {
    // `-- .` establishes a positive pathspec (everything) so the trailing
    // `:(exclude).loopr/` subtracts loopr's own state dir; without a
    // positive term the exclude-only pathspec matches nothing.
    let out = run_git(
        target,
        &["status", "--porcelain", "--", ".", ":(exclude).loopr/"],
        git_timeout,
    )
    .await?;
    if !out.status.success() {
        return Err(IntegrationError::Git(format!(
            "git status --porcelain failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
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
#[instrument(
    name = "integrator.git.is_ancestor",
    level = "debug",
    skip_all,
    fields(commit = commit, ref_name = ref_name),
    err
)]
pub(crate) async fn is_ancestor(
    target: &Path,
    commit: &str,
    ref_name: &str,
    git_timeout: Duration,
) -> Result<bool, IntegrationError> {
    let out = run_git(target, &["merge-base", "--is-ancestor", commit, ref_name], git_timeout).await?;
    if let Some(code) = out.status.code() {
        match code {
            0 => {
                debug!(commit, ref_name, ancestor = true, "integrator.git: is_ancestor ok");
                Ok(true)
            }
            1 => {
                debug!(commit, ref_name, ancestor = false, "integrator.git: is_ancestor ok");
                Ok(false)
            }
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

/// Resolve a ref via `git rev-parse --verify --quiet <rev>`. Returns
/// `Ok(Some(full_sha))` when the ref resolves, `Ok(None)` when it does
/// not (e.g., `<sha>^2` on a non-merge commit), `Err` on a subprocess
/// failure.
async fn rev_parse_opt(target: &Path, rev: &str, git_timeout: Duration) -> Result<Option<String>, IntegrationError> {
    let out = run_git(target, &["rev-parse", "--verify", "--quiet", rev], git_timeout).await?;
    if out.status.success() {
        Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()))
    } else {
        Ok(None)
    }
}

/// Find the merge commit in `bundle_head..HEAD` that actually absorbed
/// `bundle_head` via `git merge --no-ff`, identified by its SECOND
/// parent (`<merge>^2`) resolving to exactly `bundle_head`. Returns
/// `Ok(Some(sha))` for the first such merge in chronological order,
/// `Ok(None)` when no merge's second parent matches (a false-positive
/// ancestry: `bundle_head` is trivially ancestral - e.g., the
/// integration base, or absorbed by a DIFFERENT bundle's merge - and
/// must NOT be adopted), `Err` on a subprocess failure.
///
/// The `--reverse` is load-bearing: git's default order is
/// reverse-chronological, so the first line without it would be the
/// newest merge, not the one that absorbed `bundle_head`. The
/// second-parent verification is the fix for the `is_ancestor`
/// false-adopt: a trivially-ancestral `head_commit` made the old code
/// grab some other bundle's merge commit.
#[instrument(name = "integrator.git.find_adopting_merge", level = "debug", skip_all, fields(bundle_head = bundle_head), err)]
pub(crate) async fn find_adopting_merge(
    target: &Path,
    bundle_head: &str,
    git_timeout: Duration,
) -> Result<Option<String>, IntegrationError> {
    // Resolve bundle_head to its canonical full SHA so the second-parent
    // comparison is exact regardless of how the Bundle stored it.
    let bundle_head_full = match rev_parse_opt(target, bundle_head, git_timeout).await? {
        Some(sha) => sha,
        None => return Ok(None),
    };
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
    for sha in stdout.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let second_parent = rev_parse_opt(target, &format!("{sha}^2"), git_timeout).await?;
        if second_parent.as_deref() == Some(bundle_head_full.as_str()) {
            debug!(
                merge_sha = sha,
                bundle_head, "integrator.git: adopting merge confirmed by second parent"
            );
            return Ok(Some(sha.to_string()));
        }
    }
    Ok(None)
}

/// Deterministic identity stamped on every integration merge commit.
/// Pinning the identity (and dates, below) is what makes the merge SHA
/// reproducible across runs - ambient `user.name`/`user.email` vary by
/// machine and would otherwise change the commit hash.
const MERGE_IDENTITY_NAME: &str = "loopr-integrator";
const MERGE_IDENTITY_EMAIL: &str = "integrator@loopr";

/// Return a ref's committer date in strict ISO-8601 (`%cI`) via
/// `git log -1`. Used to pin the merge commit's author/committer dates
/// to the bundle head's date (a fixed fact across runs).
async fn commit_date(target: &Path, rev: &str, git_timeout: Duration) -> Result<String, IntegrationError> {
    let out = run_git(target, &["log", "-1", "--format=%cI", rev], git_timeout).await?;
    if !out.status.success() {
        return Err(IntegrationError::Git(format!(
            "git log -1 --format=%cI {rev} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Attempt `git merge --no-ff <branch> -m "Merge bundle branch <branch>"`.
/// On success returns the new `HEAD` SHA (via a follow-up `rev-parse HEAD`).
/// On non-zero exit returns the COMBINED stdout+stderr for the caller to
/// classify (git prints `CONFLICT` / `Automatic merge failed` to stdout,
/// not stderr, so the caller needs both to distinguish a genuine merge
/// conflict from a non-conflict infrastructure failure); the caller is
/// responsible for running `merge_abort` + `reset_hard`.
///
/// Determinism: the merge commit's author+committer dates are pinned to
/// the bundle branch head's committer date and its identity to a fixed
/// loopr identity, so "same bundles + same base = same Tick SHA" holds.
/// Wall-clock dates and ambient git identity otherwise made every run a
/// different merge SHA.
#[instrument(name = "integrator.git.merge_no_ff", level = "debug", skip_all, fields(branch = branch), err)]
pub(crate) async fn merge_no_ff(
    target: &Path,
    branch: &str,
    git_timeout: Duration,
) -> Result<Result<String, String>, IntegrationError> {
    let date = commit_date(target, branch, git_timeout).await?;
    let message = format!("Merge bundle branch {branch}");
    let name_cfg = format!("user.name={MERGE_IDENTITY_NAME}");
    let email_cfg = format!("user.email={MERGE_IDENTITY_EMAIL}");
    let args = [
        "-c", &name_cfg, "-c", &email_cfg, "merge", "--no-ff", branch, "-m", &message,
    ];
    let envs = [
        ("GIT_AUTHOR_DATE", date.as_str()),
        ("GIT_COMMITTER_DATE", date.as_str()),
    ];
    let out = run_git_with_env(target, &args, &envs, git_timeout).await?;
    if !out.status.success() {
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.trim().is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&stderr);
        }
        return Ok(Err(combined));
    }
    let sha = rev_parse_head(target, git_timeout).await?;
    debug!(branch, head_sha = %sha, "integrator.git: merge_no_ff ok");
    Ok(Ok(sha))
}

/// Best-effort `git merge --abort`. Errors are swallowed because the
/// merge may not have reached conflict state (e.g., a subprocess
/// failure earlier in the sequence would leave no merge to abort).
pub(crate) async fn merge_abort(target: &Path, git_timeout: Duration) {
    let _ = run_git(target, &["merge", "--abort"], git_timeout).await;
}

/// Return `true` if a merge is in progress (MERGE_HEAD resolves) via
/// `git rev-parse --verify --quiet MERGE_HEAD`. Exit 0 = a merge is
/// pending (conflicted index left by a prior crash); non-zero = no
/// merge in progress. A subprocess failure (git missing, timeout)
/// surfaces as `Err`.
///
/// Used at Phase-2 entry: a daemon crash mid-merge leaves a conflicted
/// index + MERGE_HEAD on disk, and the re-entry's `git checkout` then
/// fails ("you need to resolve your current index first"), wedging
/// integration permanently. Detecting + aborting the stale merge
/// before checkout heals that window.
#[instrument(name = "integrator.git.merge_in_progress", level = "debug", skip_all, err)]
pub(crate) async fn merge_in_progress(target: &Path, git_timeout: Duration) -> Result<bool, IntegrationError> {
    let out = run_git(target, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"], git_timeout).await?;
    let in_progress = out.status.success();
    debug!(in_progress, "integrator.git: merge_in_progress check");
    Ok(in_progress)
}

/// Best-effort `git clean -fd`. Removes untracked files and directories
/// left by validation commands that `git reset --hard` cannot restore.
/// Errors are swallowed; the rollback continues regardless.
pub(crate) async fn clean_fd(target: &Path, git_timeout: Duration) {
    let _ = run_git(target, &["clean", "-fd"], git_timeout).await;
}

/// Reset the current branch hard to the given SHA. A failure here is
/// fatal: rollback could not restore the integration branch, and the
/// daemon's worktree-crash-recovery pass at restart owns the repair.
#[instrument(name = "integrator.git.reset_hard", level = "debug", skip_all, fields(sha = sha), err)]
pub(crate) async fn reset_hard(target: &Path, sha: &str, git_timeout: Duration) -> Result<(), IntegrationError> {
    let out = run_git(target, &["reset", "--hard", sha], git_timeout).await?;
    if !out.status.success() {
        return Err(IntegrationError::Git(format!(
            "git reset --hard {sha} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    debug!(sha, "integrator.git: reset_hard ok");
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
    run_git_with_env(target, args, &[], git_timeout).await
}

#[instrument(level = "trace", skip_all, fields(target = %target.display(), git_args = ?args, git_envs = ?envs, timeout_ms = git_timeout.as_millis() as u64), err)]
async fn run_git_with_env(
    target: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    git_timeout: Duration,
) -> Result<CapturedOutput, IntegrationError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(target);
    for arg in args {
        cmd.arg(arg);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    // kill_on_drop is load-bearing here (contrast validation.rs): without
    // it, a timed-out `git merge`/`checkout` keeps running after the
    // timeout future is dropped and can land its mutation AFTER
    // `integrate` returned Err - git advances, the DB records nothing,
    // and the retry races the orphaned process. Kill the child when the
    // timeout drops the future.
    cmd.kill_on_drop(true);
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

#[cfg(test)]
mod tests;
