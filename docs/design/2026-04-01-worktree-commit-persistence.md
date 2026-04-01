# Design Document: Worktree Commit Persistence Fix

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Implementer commits vanish when work is retried because `WorktreeManager::create()` unconditionally force-deletes the `agent/<work_id>` branch before recreating the worktree. The integrator then merges an empty branch and rejects the tick for "validation failure." This renders the entire build pipeline non-functional despite all other components (tool registration, review, integration) working correctly.

## Problem Statement

### Background

Loopr uses git worktrees to isolate concurrent implementer work. Each work item `wk-xxx` gets a worktree at `.worktrees/wk-xxx` on branch `agent/wk-xxx`. The implementer writes files, runs tests, commits, and proposes a bundle referencing that branch. The integrator later merges `agent/wk-xxx` into main, runs validation, and publishes a tick.

The tool discovery loop fix (v0.1.43) resolved the infinite researcher spawning problem. The coordinator now registers tools correctly from validation-commands hints. But the lua-todo E2E run still timed out because zero code ever reached the main branch - every bundle was rejected for containing no changes.

### Problem

When a bundle is rejected and the coordinator overrides the work back to Ready, a new implementer picks up the same work. `WorktreeManager::get_or_create()` calls `create()` which runs `git branch -D agent/wk-xxx` (manager.rs:66-70, commented "Delete stale branch from a previous failed run") before recreating the worktree. This deletes the previous implementer's commits. The comment is misleading - the branch isn't "stale from a failed run"; it contains valid commits that the integrator hasn't merged yet.

The sequence:

```
1. Implementer A: creates worktree on agent/wk-xxx
2. Implementer A: writes todo.lua, commits (commit abc123 on agent/wk-xxx)
3. Implementer A: runs test -> exit 0 (file exists in worktree)
4. Implementer A: proposes bundle bd-xxx (branch_name: "agent/wk-xxx")
5. Implementer A: finishes -> cleanup removes worktree directory, keeps branch
6. Reviewer: rejects bundle (diff appears truncated/empty)
7. Coordinator: overrides wk-xxx -> Ready
8. Implementer B: picks up wk-xxx
9. WorktreeManager::create(): git branch -D agent/wk-xxx  <-- DESTROYS abc123
10. WorktreeManager::create(): git worktree add ... -b agent/wk-xxx main
11. agent/wk-xxx is now back at init commit, abc123 is orphaned
12. Integrator: merges agent/wk-xxx into main -> no-op (no new commits)
13. Integrator: runs validation -> "todo.lua: No such file or directory"
14. Tick rejected, bundle rejected, work reset -> loop repeats
```

Evidence from the lua-todo E2E run (v0.1.43):
- `git log --all` in the target repo showed only `d23dd56 init` throughout the entire run
- `git fsck --unreachable` found orphaned commit `331ad62` ("feat(todo): implement TodoStore")
- `git reflog show agent/wk-yros9` showed only one entry: "branch: Created from HEAD"
- All 14 bundles had `head_commit: null` and `final_commit: null` in the taskstore
- 41 total bundle rejections, 0 successful merges

### Goals

- Implementer commits must survive on the branch until the integrator explicitly merges or rejects them
- Work retry must not destroy commits from previous sessions that haven't been integrated
- The integrator must verify that a branch has commits before attempting to merge
- The fix must handle the normal case (single implementer) and the retry case (multiple implementers on same work)

### Non-Goals

- Changing the worktree-per-work isolation model (it's the right architecture)
- Adding multi-branch support per work item (one branch per work is sufficient)
- Modifying the reviewer's ability to see bundle diffs (separate concern)
- Changing how the coordinator decides to retry work

## Proposed Solution

### Overview

Four changes, ordered by criticality:

1. **Stop destroying branches on retry** - Replace the unconditional `git branch -D` in `create()` with a conditional check. If the branch exists, create the worktree on the existing branch (preserving its commits) rather than deleting it.

2. **Add explicit branch cleanup method** - Add `delete_branch()` to WorktreeManager for the integrator to call after a tick is published. Moves the `git branch -D` from the wrong place (worktree creation) to the right place (post-integration).

3. **Verify branch has commits before merge** - Add a pre-merge check in `merge_bundle_branches()` that compares the bundle branch HEAD against the merge base. If they're identical (no new commits), reject the bundle immediately rather than performing a no-op merge and failing at validation.

4. **Record head commit in bundle** - When `propose_bundle` creates the bundle, capture the current HEAD SHA from the worktree and store it in the bundle record. This provides an audit trail and enables the integrator to verify the branch still points to the expected commit before merging.

### Architecture

```
Current flow (broken):
  Implementer A commits abc123 on agent/wk-xxx
  -> cleanup removes worktree, branch alive
  -> Reviewer rejects bundle
  -> Coordinator: work -> Ready
  -> Implementer B: create() deletes branch, abc123 orphaned
  -> Integrator merges empty branch -> validation fails

Proposed flow:
  Implementer A commits abc123 on agent/wk-xxx
  -> cleanup removes worktree, branch alive
  -> Reviewer rejects bundle
  -> Coordinator: work -> Ready
  -> Implementer B: create() sees branch exists, creates worktree on it
  -> Implementer B inherits abc123, writes new version, commits def456
  -> Integrator: verify branch has commits beyond merge-base -> merge succeeds
```

### Implementation Plan

#### Change 1: Preserve branch on retry in WorktreeManager::create()

**File: `src/worktree/manager.rs` - `create()`**

Replace the unconditional branch deletion with branch-aware logic:

```rust
pub fn create(&self, work_id: &str, base_ref: &str) -> Result<PathBuf, WorktreeError> {
    debug!("WorktreeManager::create(key={}, base_ref={})", work_id, base_ref);
    let path = self.worktree_dir.join(work_id);
    if path.exists() {
        return Err(WorktreeError::AlreadyExists(work_id.to_string()));
    }

    let branch = format!("agent/{}", work_id);

    // Check if the branch already exists (from a previous implementer session)
    let branch_exists = Command::new("git")
        .args(["rev-parse", "--verify", &format!("refs/heads/{}", branch)])
        .current_dir(&self.repo_path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if branch_exists {
        // Branch has commits from a previous session. Create worktree
        // on the existing branch (preserving its commits).
        let output = Command::new("git")
            .args(["worktree", "add", &path.to_string_lossy(), &branch])
            .current_dir(&self.repo_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::GitCommand(stderr.to_string()));
        }
    } else {
        // No existing branch - create fresh from base_ref
        let output = Command::new("git")
            .args([
                "worktree", "add",
                &path.to_string_lossy(),
                "-b", &branch,
                base_ref,
            ])
            .current_dir(&self.repo_path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorktreeError::GitCommand(stderr.to_string()));
        }
    }

    // Verify the branch was actually checked out
    let verify = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(&path)
        .output()?;
    let actual_branch = String::from_utf8_lossy(&verify.stdout).trim().to_string();
    if actual_branch != branch {
        return Err(WorktreeError::GitCommand(format!(
            "worktree created but branch mismatch: expected '{}', got '{}'",
            branch, actual_branch
        )));
    }

    Ok(path)
}
```

**Key design decisions:**

- If the branch exists, we reuse it. The new implementer inherits the previous session's commits and can build on them or amend them.
- No `git branch -D` anywhere in the normal path. The branch is only deleted by explicit cleanup after the integrator publishes a tick (existing behavior at line 168-169 comment).
- The `base_ref` parameter is only used for the initial branch creation (no existing branch).
- Note: implementers enter via `get_or_create()`, which returns early if the worktree directory already exists (`.git` file present). The branch-exists check in `create()` fires only when the directory was cleaned up but the branch survived - exactly the retry scenario.

#### Change 2: Add `delete_branch()` method for explicit branch cleanup

**File: `src/worktree/manager.rs`**

Add a method the integrator can call after a tick is published to clean up the branch:

```rust
/// Delete the agent branch for a work item. Called by the Integrator
/// after a Tick is published (commits are safely on main).
pub fn delete_branch(&self, work_id: &str) -> Result<(), WorktreeError> {
    let branch = format!("agent/{}", work_id);
    let output = Command::new("git")
        .args(["branch", "-D", &branch])
        .current_dir(&self.repo_path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Branch may already be deleted - not an error
        if !stderr.contains("not found") {
            return Err(WorktreeError::GitCommand(stderr.to_string()));
        }
    }
    Ok(())
}
```

#### Change 3: Pre-merge commit verification in integrator

**File: `src/agents/integrator.rs` - `merge_bundle_branches()`**

Before merging, verify the branch has commits beyond the merge base. Currently `git merge --no-ff <branch>` where the branch points to the same commit as HEAD exits with code 0 and message "Already up to date" - a silent no-op that the existing success check doesn't catch:

```rust
fn merge_bundle_branches(
    repo_path: &std::path::Path,
    bundle_branches: &[String],
) -> Result<String> {
    for branch in bundle_branches {
        // Verify the branch has commits beyond the merge base
        let merge_base = std::process::Command::new("git")
            .args(["merge-base", "HEAD", branch])
            .current_dir(repo_path)
            .output()
            .map_err(|e| eyre!("git merge-base failed: {}", e))?;

        let branch_head = std::process::Command::new("git")
            .args(["rev-parse", branch])
            .current_dir(repo_path)
            .output()
            .map_err(|e| eyre!("git rev-parse {} failed: {}", branch, e))?;

        let base_sha = String::from_utf8_lossy(&merge_base.stdout).trim().to_string();
        let head_sha = String::from_utf8_lossy(&branch_head.stdout).trim().to_string();

        if base_sha == head_sha {
            return Err(eyre!(
                "branch {} has no commits beyond merge base (both at {}). \
                 The implementer's commits may have been lost.",
                branch, base_sha
            ));
        }

        // Existing merge logic
        let output = std::process::Command::new("git")
            .args([
                "merge", "--no-ff", branch,
                "-m", &format!("Merge bundle branch {}", branch),
            ])
            .current_dir(repo_path)
            .output()
            .map_err(|e| eyre!("git merge {} failed to execute: {}", branch, e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::process::Command::new("git")
                .args(["merge", "--abort"])
                .current_dir(repo_path)
                .output();
            return Err(eyre!("git merge {} failed (aborted): {}", branch, stderr));
        }
    }

    let sha_output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .map_err(|e| eyre!("git rev-parse HEAD failed: {}", e))?;

    Ok(String::from_utf8_lossy(&sha_output.stdout).trim().to_string())
}
```

#### Change 4: Capture head commit in propose_bundle

**File: `src/agents/executor.rs` - `ProposeBundle` handler**

After the auto-commit, capture the worktree HEAD and include it in the bundle creation request:

```rust
// After auto-commit, capture the current HEAD SHA
let head_sha = tokio::process::Command::new("git")
    .args(["rev-parse", "HEAD"])
    .current_dir(worktree_path)
    .output()
    .await
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

let mut params = serde_json::json!({
    "work_id": wi_id,
    "branch_name": branch_name,
    "claims": claims,
    "description": description,
});
if let Some(sha) = &head_sha {
    params["head_commit"] = serde_json::Value::String(sha.clone());
}
```

**File: `src/domain/bundle.rs`**

Add `head_commit` field to the Bundle struct (with `#[serde(default)]` for backward compat).

### Data Model

One new field on `Bundle`:
- `head_commit: Option<String>` - SHA of the worktree HEAD at bundle proposal time. Used for audit and pre-merge verification.

### API Design

One new method on `WorktreeManager`:
- `delete_branch(work_id: &str)` - Explicit branch cleanup after integration. Called by the integrator after tick publication.

No new IPC methods. All changes are internal to the worktree manager, executor, and integrator.

## Alternatives Considered

### Alternative 1: Force-push worktree commits to a remote branch

- **Description:** After committing in the worktree, run `git push origin agent/wk-xxx` to persist commits to a remote branch that survives worktree deletion.
- **Pros:** Commits are durable even if the local branch is deleted.
- **Cons:** Loopr targets local repos without remotes. Adds network dependency. Introduces push/pull complexity. The target repo may not have a remote configured.
- **Why not chosen:** Over-engineered. The local branch IS the persistence mechanism - we just need to stop deleting it.

### Alternative 2: Copy commits to a separate refs namespace

- **Description:** After committing, run `git update-ref refs/bundles/bd-xxx HEAD` to create an immutable ref that survives branch deletion.
- **Pros:** Completely decouples bundle refs from agent branches. No risk of accidental deletion.
- **Cons:** Adds a parallel ref management system. The integrator would need to merge from `refs/bundles/` instead of branch names. More complexity than needed.
- **Why not chosen:** The branch-based model is correct. The bug is a single line (`git branch -D`) that shouldn't be there. Don't add infrastructure to work around a one-line fix.

### Alternative 3: Detached HEAD with explicit ref storage

- **Description:** Create worktrees in detached HEAD mode. After commit, manually update the branch ref.
- **Pros:** Separates worktree lifecycle from branch lifecycle.
- **Cons:** Detached HEAD is error-prone. LLM agents may interact poorly with detached HEAD warnings. Adds manual ref management.
- **Why not chosen:** The current branch-based approach is correct and simpler.

## Technical Considerations

### Dependencies

None. All changes use existing git commands and Loopr types.

### Performance

Negligible. One additional `git rev-parse --verify` call per worktree creation. One additional `git merge-base` + `git rev-parse` per merge attempt.

### Security

No new security considerations. The validation gate (v0.1.42) still validates all tool registrations. The integrator still runs validation commands after merge.

### Testing Strategy

**Change 1 - Branch preservation:**
1. **Unit test:** `create()` with existing branch reuses it (no deletion)
2. **Unit test:** `create()` without existing branch creates fresh
3. **Integration test:** Implementer A commits, cleanup removes worktree, Implementer B gets worktree with A's commits still on branch
4. **Integration test:** `get_or_create()` with existing worktree directory returns it; with existing branch but no directory, creates worktree on existing branch

**Change 2 - Branch deletion method:**
5. **Unit test:** `delete_branch()` removes the branch
6. **Unit test:** `delete_branch()` on non-existent branch returns Ok (idempotent)

**Change 3 - Pre-merge verification:**
7. **Unit test:** `merge_bundle_branches()` rejects branch with no commits beyond merge base
8. **Unit test:** `merge_bundle_branches()` succeeds for branch with real commits
9. **Integration test:** Full cycle - commit in worktree, cleanup, merge succeeds

**Change 4 - Head commit capture:**
10. **Unit test:** `propose_bundle` includes `head_commit` in bundle creation params
11. **Unit test:** Bundle serde roundtrip with `head_commit` field
12. **Unit test:** Backward compat - old JSON without `head_commit` deserializes with None

**E2E:**
13. Re-run lua-todo E2E, verify: commits persist on branch, integrator merges successfully, files appear in target repo, phase 1 completes

### Rollout Plan

Single deployment. Changes are ordered by criticality:
1. Branch preservation in `create()` (critical - fixes the root cause)
2. Pre-merge verification in integrator (safety net - catches the symptom)
3. Head commit capture in propose_bundle (audit trail - enables debugging)
4. `delete_branch()` method (housekeeping - prevents branch accumulation)

No feature flag needed. All changes are backward compatible via `#[serde(default)]`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Reusing a corrupted/conflicted branch from a previous session | Low | Medium | The implementer starts from the existing branch state. If the previous session left the branch in a bad state, the new implementer will see errors and can handle them. Worst case: coordinator overrides work to Ready and we get one more retry cycle. |
| Branch accumulation (branches never deleted) | Low | Low | The `delete_branch()` method provides explicit cleanup. The integrator should call it after publishing a tick. Branches for abandoned work can be cleaned up by the coordinator. |
| Merge-base check false positive (branch has commits but they match base) | Very Low | Low | This would only happen if someone force-pushed the branch to match main, which no Loopr component does. |
| Backward compat with existing taskstore data missing `head_commit` | N/A | N/A | `#[serde(default)]` handles missing field as None. |
| Implementer B's changes conflict with Implementer A's leftover commits | Low | Low | The implementer writes files and commits. If A's commits touched the same files, B's writes will naturally replace them. Git commit captures the full file content, not a diff. |
| Reused branch is stale (tick published between sessions) | Medium | Medium | The existing `refresh()` method handles this via rebase. After creating the worktree on the existing branch, the executor should call `refresh()` if a newer tick exists. The staleness detection already exists in the implementer loop. |
| Two implementers race on same work, both call `create()` | Low | Low | The `get_or_create()` TOCTOU handler already covers this. If one racer creates the worktree first, the other gets `AlreadyExists` and falls through to the path-check. No change needed. |

## Open Questions

- [x] ~~Should we delete the branch or reuse it?~~ Reuse. The branch is the persistence mechanism.
- [x] ~~Should the integrator verify branch has commits?~~ Yes. Defense in depth against silent no-op merges.
- [x] ~~Should the integrator call `delete_branch()` after tick publication, or should there be a separate cleanup sweep?~~ Both. The integrator calls `delete_branch()` for merged branches after tick publication. The coordinator calls it for abandoned work (work status = Abandoned). This covers both lifecycle endpoints.
- [x] ~~Should `get_or_create()` call `refresh()` (rebase) on an existing branch to pick up new ticks?~~ No. The staleness detection already exists in the implementer loop (`drain_tick_published()`). If a tick was published, the implementer rebases at the next safe point. Adding rebase to worktree creation would duplicate this logic and could mask conflicts.

## References

- `src/worktree/manager.rs` - WorktreeManager, `create()` (the bug), `cleanup()`, `get_or_create()`
- `src/agents/executor.rs` - Commit handler (line 762), ProposeBundle handler (line 783), worktree cleanup (line 294)
- `src/agents/integrator.rs` - `merge_bundle_branches()` (line 828)
- `src/domain/bundle.rs` - Bundle struct
- `docs/design/2026-04-01-tool-discovery-loop-fix.md` - Previous fix that cleared the path to discovering this bug
- `docs/design/2026-02-25-orchestration-spine.md` - Original worktree isolation design
