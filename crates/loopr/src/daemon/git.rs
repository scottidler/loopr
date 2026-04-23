//! Small git helpers the daemon invokes directly (not via `tools`).
//!
//! These are infrastructure-level git subprocess wrappers, not agent-callable
//! capabilities. Kept out of `worktree` because they operate on the target
//! repo's main working tree rather than a sibling worktree.

use std::path::Path;

use tokio::process::Command;

use domain::PlanId;

/// Create `loopr/plan-<plan-id>` from the target's current HEAD if it does
/// not already exist. Idempotent: a second call with the same `plan_id`
/// returns Ok without error.
///
/// Called from `handle_plan_create` BEFORE persisting the Plan + Works so
/// that a git failure (e.g. fresh repo with no HEAD) does not leave orphan
/// records in the taskstore. The branch's HEAD at creation is the
/// deterministic base SHA the Integrator's "same bundles + same base = same
/// Tick SHA" contract requires.
///
/// Uses `git -C <target>` per-argument (no shell), so user-controlled
/// content in `plan_id` is constrained by the `PlanId` type (5-char base36
/// suffix with a `pl-` prefix; no shell metacharacters possible).
pub async fn ensure_integration_branch(target: &Path, plan_id: &PlanId) -> Result<(), std::io::Error> {
    let branch = format!("loopr/plan-{plan_id}");

    // Existence check: `git rev-parse --verify <branch>` exits 0 if the
    // branch exists, non-zero otherwise. No error on non-existence.
    let verify = Command::new("git")
        .arg("-C")
        .arg(target)
        .args(["rev-parse", "--verify", "--quiet", &branch])
        .output()
        .await?;
    if verify.status.success() {
        return Ok(());
    }

    // Create the branch at current HEAD. No checkout; the branch ref is
    // sufficient for the Integrator to check out later.
    let create = Command::new("git")
        .arg("-C")
        .arg(target)
        .args(["branch", &branch, "HEAD"])
        .output()
        .await?;
    if !create.status.success() {
        return Err(std::io::Error::other(format!(
            "git branch {branch} failed: {}",
            String::from_utf8_lossy(&create.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
