# Design Document: Stale Worktree on Retry

**Author:** Scott A. Idler
**Date:** 2026-04-10
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

When a bundle is rejected and the implementer retries, `WorktreeManager::create_branch` reuses the existing `agent/wk-xxxxx` branch without rebasing onto the current integration branch. The implementer works from a stale snapshot that lacks sibling works' merged code, overwrites shared files, the reviewer rejects, and the cycle repeats until the lifeguard kills the session. This design doc specifies the fix: rebase existing agent branches onto the current `base_ref` during worktree creation.

## Problem Statement

### Background

Loopr's worktree lifecycle works as follows:

1. **First attempt:** `lifecycle.rs:59` resolves `base_ref` to `integration/<plan_id>` via `resolve_worktree_base_for()`, then calls `get_or_create_branch(work_id, base_ref)`. Since no branch exists, `create_branch` runs `git worktree add <path> -b agent/<work_id> <base_ref>` - correct.

2. **Bundle rejection:** The reviewer rejects, the worktree is cleaned up via `cleanup()`, but the branch `agent/<work_id>` is preserved (by design - the integrator may need it).

3. **Retry:** A new implementer session starts. `lifecycle.rs` again resolves `base_ref` to the current tip of `integration/<plan_id>` (which now includes sibling merges). But `create_branch` sees the branch already exists and runs `git worktree add <path> <branch>` - **ignoring `base_ref` entirely** (manager.rs:77-81).

### Problem

The retry worktree checks out the old branch as-is. If sibling works (e.g., wk-hr5na adding 6 tests to `test_api.py`) were merged into the integration branch between the first attempt and the retry, the implementer cannot see them. It writes `test_api.py` from scratch with only its 2 new tests, the reviewer sees 6 tests vanish and rejects, and the cycle repeats.

This was observed in the `python-api` E2E run where wk-88g0s death-looped through 2 rejected bundles and 5 identical implementer iterations. The learnings system told the agent "don't delete existing tests" but the agent literally could not see the tests it was told to preserve.

**This is a system bug, not an agent bug.** No amount of prompt engineering or learnings can fix an implementer that lacks visibility into sibling work.

### Goals

- Implementer retry worktrees include all sibling work merged into the integration branch
- The fix is contained within the worktree manager - no changes to agent logic or lifecycle
- Rebase conflicts (if any) are detected and handled gracefully
- Existing tests continue to pass; new test proves the rebase-on-retry behavior

### Non-Goals

- Decomposer AC validation (the BOOKMARK_DB_PATH vs DATABASE_PATH env var mismatch is a separate issue)
- Changing how the integrator calls refresh (it already works correctly for its use case)
- Worktree refresh for reviewer worktrees (reviewers don't retry in the same way)

## Proposed Solution

### Overview

Add a rebase step inside `create_branch` when the branch already exists. After `git worktree add <path> <branch>`, run `git rebase <base_ref>` inside the new worktree. This brings the existing agent branch up to date with the current integration branch tip.

### Implementation

**File:** `src/worktree/manager.rs`

**Change in `create_branch`:** After the existing-branch path (line 77-81) successfully creates the worktree, call `self.refresh(work_id, base_ref)` to rebase the branch onto the new base.

```rust
let output = if branch_exists {
    debug!("branch {} exists, creating worktree on existing branch", branch);
    let out = Command::new("git")
        .args(["worktree", "add", &path.to_string_lossy(), &branch])
        .current_dir(&self.repo_path)
        .output()?;
    if out.status.success() {
        // Rebase existing branch onto new base_ref so retried
        // implementers see sibling work merged since the last attempt.
        if let Err(e) = self.refresh(work_id, base_ref) {
            warn!("rebase onto {} failed for {}: {} - branch may be stale",
                  base_ref, work_id, e);
        }
    }
    out
} else {
    // ... unchanged fresh-branch path ...
};
```

### Rebase Conflict Handling

If `refresh` (rebase) fails - e.g., the implementer's previous attempt made changes that conflict with newly merged sibling work - `refresh` already aborts the rebase (`git rebase --abort`) and returns an error. The current code logs a warning and proceeds with the stale branch. This is acceptable because:

1. Conflicts on retry are rare - the implementer's previous attempt was rejected, so its changes are likely wrong anyway
2. A stale branch that fails rebase is no worse than the current behavior (always stale)
3. The implementer will produce a new diff that the reviewer evaluates fresh

An alternative for conflict handling: delete the branch and recreate from `base_ref`, losing the previous attempt's commits entirely. This is cleaner for the retry case since those commits were already rejected.

### Alternative Conflict Strategy: Branch Reset

If rebase fails, reset the branch to `base_ref` instead of keeping it stale:

```rust
if let Err(e) = self.refresh(work_id, base_ref) {
    warn!("rebase failed for {} - resetting branch to {}: {}", work_id, base_ref, e);
    // Hard reset to base_ref - previous commits were rejected anyway
    let reset = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "reset", "--hard", base_ref])
        .output();
    if let Err(re) = reset {
        warn!("branch reset also failed for {}: {}", work_id, re);
    }
}
```

**Recommendation:** Use the reset fallback. A rejected implementer's commits have no value - starting fresh from the integration tip is strictly better than staying stale.

### Testing Strategy

**New unit test in `manager.rs`:** `test_create_rebases_existing_branch_onto_new_base`

Setup:
1. `init_test_repo` with initial commit on main
2. Create worktree for `wi-001` branching from HEAD (simulates first attempt)
3. Commit a file `hello.txt` in the worktree (simulates implementer work)
4. Cleanup the worktree (simulates session end)
5. Commit a new file `sibling.txt` on main (simulates sibling work merged into integration branch)
6. Create worktree for `wi-001` again with `base_ref` = current HEAD (simulates retry)
7. Assert: `sibling.txt` exists in the new worktree (rebase brought it in)
8. Assert: `hello.txt` also exists (previous implementer commits preserved via rebase)

**Edge case test:** `test_create_rebase_conflict_resets_to_base`

Setup:
1. `init_test_repo`, create worktree for `wi-001`, modify `README.md`, commit, cleanup
2. On main, also modify `README.md` differently (creates conflict)
3. Create worktree for `wi-001` with new `base_ref`
4. Assert: worktree has main's version of `README.md` (reset fallback used)
5. Assert: `hello.txt` from the previous attempt is gone (reset cleared it)

## Alternatives Considered

### Alternative 1: Refresh in `get_or_create_branch` instead of `create_branch`

- **Description:** Move the refresh call to `get_or_create_branch`, after `create_branch` returns for the existing-branch case.
- **Pros:** Keeps `create_branch` simpler.
- **Cons:** `get_or_create_branch` already handles TOCTOU races and path validation - adding rebase logic muddies its purpose. Also, `create_branch` is called directly from the IPC handler (`handlers/worktree.rs:67`), so that path would not get the fix.
- **Why not chosen:** The fix belongs where the decision is made - `create_branch` is the function that knows it's reusing an existing branch and ignoring `base_ref`.

### Alternative 2: Delete and recreate the branch on retry

- **Description:** When `branch_exists` is true, delete the branch (`git branch -D`), then fall through to the fresh-branch creation path.
- **Pros:** Simple, no rebase needed, guaranteed clean slate.
- **Cons:** Loses any commits from the previous attempt. While those commits were rejected, they might contain partial work that a smarter implementer could build on. Also requires worktree prune since the branch is linked to a worktree registration.
- **Why not chosen:** Rebase-then-reset-on-conflict achieves the same cleanliness in the conflict case while preserving commits in the happy path (no conflict). Best of both worlds.

### Alternative 3: Refresh in lifecycle.rs after `get_or_create_branch`

- **Description:** Add a `worktree_mgr.refresh(key, &base_ref)` call in `lifecycle.rs` right after the worktree is created.
- **Pros:** No changes to the worktree manager.
- **Cons:** The lifecycle code would need to distinguish first-attempt from retry to avoid a useless rebase on fresh branches. Also, the IPC-based worktree.create handler would remain unfixed.
- **Why not chosen:** The worktree manager should handle its own consistency - callers should not need to compensate for its behavior.

## Technical Considerations

### Dependencies

None - the fix uses `self.refresh()` which already exists and is tested.

### Performance

One additional `git rebase` call per retry. Negligible - rebase on a few commits over a small codebase is sub-second.

### Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Rebase conflict on retry | Low | Low | Reset fallback drops stale commits and starts fresh from integration tip |
| Rebase succeeds but produces broken code | Very low | Medium | Reviewer catches it as normal; no worse than current behavior |
| `refresh` error logging noise | Low | None | Warning is appropriate - operator visibility into worktree staleness |

## Open Questions

- [ ] Should the branch reset on conflict be `--hard` to `base_ref`, or should we delete and recreate the branch entirely? (Recommendation: `reset --hard` is simpler and avoids branch deletion/recreation dance)

## References

- `src/worktree/manager.rs:59-108` - `create_branch` (the bug)
- `src/worktree/manager.rs:160-181` - `refresh` (the existing rebase mechanism)
- `src/agents/executor/lifecycle.rs:58-116` - worktree setup in agent lifecycle
- `src/agents/executor/util.rs:37-69` - `resolve_worktree_base_for` (correctly resolves base_ref)
- `src/agents/integrator.rs:403-409` - integrator's existing use of `worktree.refresh`
- E2E `python-api` run report (2026-04-10) - death loop on wk-88g0s
