# Design Document: Bundle State Alignment

**Author:** Scott A. Idler
**Date:** 2026-04-10
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

After six E2E rounds spanning v0.1.88 through v0.1.114, the same class of failure keeps appearing: the implementer and reviewer see different views of the codebase, causing rejection loops that waste agent sessions and stall the pipeline. The existing noop bundle lifecycle fix (2026-04-10) addressed empty paths but missed the deeper problem: worktree branch reuse creates a state where the implementer sees committed code from a previous session while the reviewer reads the integration branch HEAD, which lacks that code. This design closes the gap with four targeted fixes and introduces a deterministic integration test harness to prevent regressions.

## Problem Statement

### Background

The worktree-to-bundle-to-review-to-integrate pipeline is the critical path in loopr. Every Work item passes through it. Six rounds of E2E testing have exposed failures at different handoff points, but all share a common root: **the implementer's view of the filesystem diverges from the reviewer's view**, and the system has no mechanism to detect or correct this divergence.

The existing noop bundle lifecycle design doc (2026-04-10-noop-bundle-lifecycle.md, Implemented) fixed one variant: noop bundles with `paths: []` gave the reviewer nothing to review. After that fix, noop bundles carry `noop_paths` and the reviewer reads those files from the repo.

But the fix reads files from `repo_path` (integration branch HEAD). When the implementer's worktree is checked out on a preserved agent branch with commits that differ from the integration branch, the reviewer sees a different version of the file than the implementer.

### The Failure Chain (observed 2026-04-10 E2E)

```
1. Implementer A commits CRUD functions to agent/wk-84cy3 (commit 1fed55f)
   -> bundle bd-1kmar proposed with head_commit=1fed55f

2. Integrator rejects bd-1kmar (stale base tick - wk-i24ye merged first)
   -> agent/wk-84cy3 branch preserved (intentional, for retry)
   -> wk-84cy3 reset to Ready

3. Implementer B gets worktree via get_or_create_branch()
   -> branch agent/wk-84cy3 exists -> reuses it (preserves 1fed55f)
   -> Implementer B reads database.py -> 113 lines (CRUD code present)
   -> "nothing to commit" -> proposes NOOP bundle (branch_name="", head_commit=None)

4. Reviewer gets noop bundle
   -> reads database.py from repo_path (integration branch HEAD)
   -> 45 lines (only foundation functions, no CRUD)
   -> REJECTS: "CRUD functions absent"

5. Cycle repeats 3x -> max_bundle_rejections -> wk-84cy3 Blocked
   -> All downstream works stuck in Pending
```

### Two failure modes from independent E2E runs

| Run | Trigger | Symptom | Root cause |
|-----|---------|---------|------------|
| Claude (2026-04-10) | Stale base tick rejection | Implementer sees 113 lines, reviewer sees 45 | Worktree branch reuse + noop bundle reads integration HEAD |
| Gemini (separate run) | Missing phase document | Implementer crashes before bundle creation | No session failure counter; coordinator retries infinitely |

Both lead to the same terminal state: Work stuck, pipeline stalled.

### Goals

- Eliminate the false-noop problem: when a branch has divergent commits, propose a normal bundle
- After stale rejection, align the agent branch with the current integration HEAD
- Add a session-failure counter independent of the bundle rejection counter
- Create deterministic integration tests for the worktree->bundle->review->integrate handoff

### Non-Goals

- Rewriting the worktree manager or bundle lifecycle (architecture is sound)
- Eliminating noop bundles entirely (they have legitimate uses for scaffold files)
- Scoped per-work-item validation (valuable but a separate design doc)
- Changing the reviewer's review model (diff-based vs filesystem)

## Proposed Solution

### Overview

Four fixes targeting the specific handoff failures, plus a test harness.

1. **Eliminate false noops** - detect when agent branch diverges from integration HEAD and force a normal bundle
2. **Rebase agent branch after stale rejection** - align preserved work with current integration state
3. **Add max_session_failures counter** - catch crash-before-bundle loops independently of bundle rejections
4. **Deterministic integration test harness** - exercise the handoff path without LLM, Docker, or 20-minute E2E runs

### Fix 1: Eliminate False Noops

**Location:** `src/agents/executor/action/bundle.rs` lines 22-55

**Problem:** The implementer determines "noop" based on git status (nothing to commit). But the agent branch may have commits from a previous session that haven't been integrated. From the integration branch's perspective, this is NOT a noop - there's real work on the agent branch.

**Current code (line 22):**
```rust
let is_noop = noop_reason.is_some();
```

**Fix:** When the implementer proposes a noop, check whether the agent branch actually differs from the integration branch. If it does, **auto-convert** the noop to a normal bundle by capturing `branch_name` and `head_commit`. Rejecting would trap the implementer in a loop - the code is already committed, there's nothing to add.

```rust
let mut is_noop = noop_reason.is_some();

if is_noop {
    // Check if the agent branch has commits that differ from the
    // integration branch. Compare tree-ish refs, not working tree.
    // LOOPR_BASE_REF is set by the executor to the integration branch
    // HEAD that this worktree was created from / rebased onto.
    let base_ref = std::env::var("LOOPR_BASE_REF")
        .unwrap_or_else(|_| "HEAD".to_string());
    let diff_check = tokio::process::Command::new("git")
        .args(["diff", "--quiet", &base_ref, "HEAD"])
        .current_dir(worktree_path)
        .output()
        .await;
    if let Ok(output) = diff_check {
        if !output.status.success() {
            // Exit code 1 = there ARE differences between commits
            ctx.info(
                "Auto-converting false noop to normal bundle: agent branch \
                 has commits that differ from integration HEAD"
            );
            is_noop = false;
            // Fall through to normal bundle path below, which captures
            // branch_name and head_commit automatically
        }
    }
}
```

**How LOOPR_BASE_REF gets set:** The executor already knows the `base_ref` when calling `get_or_create_branch()`. It sets `LOOPR_BASE_REF` in the implementer's process environment before spawning the agent. This is the integration branch HEAD SHA that the worktree was created from (or rebased onto after Fix 2).

**Why auto-convert instead of reject:** If we reject the noop, the implementer can't do anything useful - the code is already committed on the branch, `git status` is clean. The implementer would loop: read file -> nothing to commit -> propose noop -> rejected -> repeat. Auto-converting captures the existing branch_name and head_commit, sending the bundle through the normal review path where the reviewer gets a real `git diff`.

**Why this works:** After auto-conversion, the bundle has `branch_name = "agent/<work_id>"` and `head_commit = <SHA>`. The reviewer uses `git diff HEAD agent/<work_id>` to see the actual changes. No context divergence. The implementer's session completes successfully without a second attempt.

### Fix 2: Rebase Agent Branch After Stale Rejection

**Location:** `src/agents/integrator.rs` in `reset_work_after_bundle_rejection()`

**Problem:** When a bundle is rejected for stale base tick, the agent branch is preserved but based on an old integration HEAD. The next implementer session reuses this branch, finds committed code, proposes a noop, and the reviewer rejects because the integration HEAD doesn't have that code.

**Current behavior (lines 1446-1484):** Resets work to Ready, creates Learning. Does not touch the agent branch.

**Fix:** After resetting work to Ready, rebase the agent branch onto the current integration HEAD. If the rebase fails (conflict), delete the branch so the next session starts clean.

```rust
fn reset_work_after_bundle_rejection(&self, work_id: &str, reason: &str) {
    // ... existing: transition work to Ready, create Learning ...

    // Rebase the agent branch onto current integration HEAD to prevent
    // false-noop loops. If rebase fails (conflict), delete the branch
    // so the next implementer starts from a clean state.
    let branch = format!("agent/{}", work_id);
    let repo_path = &self.ctx.stores.config.project.repo_path;

    // Check if agent branch exists
    let branch_exists = std::process::Command::new("git")
        .args(["rev-parse", "--verify", &format!("refs/heads/{}", branch)])
        .current_dir(repo_path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !branch_exists {
        return; // No branch to rebase, next session starts fresh
    }

    // Determine integration branch HEAD by walking Work -> Phase -> Spec -> Plan
    // to get the plan_id, then constructing "integration/<plan_id>".
    let integ_ref = {
        let works = self.ctx.stores.read_works().ok();
        let phases = self.ctx.stores.read_phases().ok();
        let specs = self.ctx.stores.read_specs().ok();
        let plan_id = works.as_ref()
            .and_then(|w| w.get(work_id))
            .and_then(|w| phases.as_ref()?.get(&w.parent_id))
            .and_then(|ph| specs.as_ref()?.get(&ph.parent_id))
            .map(|sp| sp.parent_id.clone());
        match plan_id {
            Some(pid) => format!("integration/{}", pid),
            None => {
                self.ctx.warn(&format!(
                    "Cannot resolve integration branch for {} - skipping rebase",
                    work_id
                ));
                return;
            }
        }
    };

    // Attempt rebase
    let rebase_result = std::process::Command::new("git")
        .args(["rebase", &integ_ref, &branch])
        .current_dir(repo_path)
        .output();

    match rebase_result {
        Ok(output) if output.status.success() => {
            self.ctx.info(&format!(
                "Rebased {} onto {} after bundle rejection",
                branch, integ_ref
            ));
        }
        _ => {
            // Rebase failed (conflict) - abort and delete the branch
            let _ = std::process::Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(repo_path)
                .output();
            let _ = std::process::Command::new("git")
                .args(["branch", "-D", &branch])
                .current_dir(repo_path)
                .output();
            self.ctx.warn(&format!(
                "Rebase of {} failed (conflict) - deleted branch; next session starts clean",
                branch
            ));
        }
    }
}
```

**Why this works:** After rebase, the agent branch has the previous session's commits applied on top of the current integration HEAD. The next implementer session finds the branch, reuses it, and either:
- Finds the code is already correct -> proposes a normal bundle (Fix 1 prevents false noop) -> reviewer sees the real diff -> approves
- Finds the rebased code needs fixes -> makes changes -> proposes a normal bundle -> reviewer sees the real diff

If rebase fails due to conflict, the branch is deleted. The next session starts from the integration HEAD. The previous work is lost, but it conflicted and couldn't be cleanly applied anyway.

### Fix 3: max_session_failures Counter on Work

**Location:** `src/domain/work.rs` (field) + `src/agents/executor/lifecycle.rs` (increment) + `src/agents/coordinator/reconcile.rs` (gate)

**Problem (Gemini finding):** When an implementer crashes during initialization (e.g., missing phase document), no bundle is created and the `max_bundle_rejections` counter doesn't increment. The coordinator blindly respawns the agent, creating an infinite crash loop.

**Fix:** Add a `session_failure_count: u32` field to Work. Increment it whenever an agent session ends in Failed or Cancelled status for that work. Gate promotion in the reconciler.

```rust
// src/domain/work.rs
pub struct Work {
    // ... existing fields ...
    /// Number of consecutive agent session failures (crash/cancel before
    /// bundle creation). Independent of max_bundle_rejections.
    #[serde(default)]
    pub session_failure_count: u32,
}
```

```rust
// src/agents/executor/lifecycle.rs - in the terminal state handling
// After agent loop exits with Failed or Cancelled status:
if matches!(terminal_status, AgentStatus::Failed | AgentStatus::Cancelled) {
    if let Some(ref wid) = work_id {
        // Increment session_failure_count under the work write lock,
        // following the same pattern as attempt_count (read-modify-write
        // under a single lock acquisition to avoid TOCTOU).
        let should_block = {
            let mut works = stores.write_works()?;
            if let Some(work) = works.get_mut(wid.as_str()) {
                work.session_failure_count += 1;
                // Persist to TaskStore while still holding the lock
                if let Some(ref store) = stores.store {
                    if let Ok(mut s) = store.lock() {
                        let _ = s.update(work.clone());
                    }
                }
                work.session_failure_count >= max_session_failures
            } else {
                false
            }
        }; // lock dropped

        if should_block {
            tracing::error!(
                "Work {} reached max_session_failures ({}) - transitioning to Blocked",
                wid, max_session_failures
            );
            let _ = bridge.request(
                "work.transition",
                serde_json::json!({
                    "id": wid,
                    "target_status": "Blocked",
                    "role": "coordinator",
                    "override": true,
                }),
            );
        }
    }
}

// On successful completion, reset session_failure_count to 0.
// Only consecutive failures trigger blocking.
if terminal_status == AgentStatus::Completed {
    if let Some(ref wid) = work_id {
        let mut works = stores.write_works()?;
        if let Some(work) = works.get_mut(wid.as_str()) {
            if work.session_failure_count > 0 {
                work.session_failure_count = 0;
                if let Some(ref store) = stores.store {
                    if let Ok(mut s) = store.lock() {
                        let _ = s.update(work.clone());
                    }
                }
            }
        }
    }
}
```

**Config:** `max-session-failures: 3` in `loopr.yml` under `strategy:`. Default 3. Follows the same convention as `max-bundle-rejections` (kebab-case config key, snake_case Rust field).

### Fix 4: Deterministic Integration Test Harness

**Location:** `src/tests/integration/handoff.rs` (new file)

**Problem:** The only feedback loop for the worktree->bundle->review->integrate path is 20-minute E2E runs. Each run finds one bug. Six rounds, six different bugs, same area. This is unsustainable.

**Fix:** Create a deterministic test harness that exercises the handoff path without LLM, Docker, or real agent sessions. Use the existing test infrastructure (tempdir, in-memory stores) with synthetic git repos.

**Test scenarios to cover:**

| # | Scenario | Expected outcome |
|---|----------|-----------------|
| 1 | Normal bundle: implementer commits, reviewer reviews diff, integrator merges | Work -> Done |
| 2 | Stale rejection: bundle rejected, agent branch rebased, re-proposed | Work -> Done (second attempt) |
| 3 | Stale rejection + rebase conflict: branch deleted, fresh session | Work -> Done (third attempt, clean) |
| 4 | False noop: agent branch has commits, implementer proposes noop | ActionError returned, implementer forced to normal bundle |
| 5 | True noop: scaffold file satisfies AC, no agent branch divergence | Noop bundle approved, Work -> Done |
| 6 | Session crash before bundle: 3 consecutive crashes | Work -> Blocked via max_session_failures |
| 7 | Mixed tick: one noop + one normal bundle in same tick | Normal merge proceeds, noop skipped, both works advance |
| 8 | Concurrent implementers: two works on same phase, stale rejection on one | Rejected work rebased, other work unaffected |

**Test structure:**

```rust
#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    /// Create a minimal git repo with an integration branch
    fn setup_repo() -> (TempDir, PathBuf) { ... }

    /// Create a WorktreeManager pointing at the test repo
    fn setup_worktree_manager(repo: &Path) -> WorktreeManager { ... }

    /// Simulate an implementer session: create worktree, write files, commit
    fn simulate_implement(mgr: &WorktreeManager, work_id: &str, files: &[(&str, &str)]) -> String { ... }

    /// Simulate the reviewer's context building for a bundle
    fn simulate_reviewer_context(stores: &Stores, bundle_id: &str) -> ReviewerContext { ... }

    /// Simulate integrator merge cycle
    fn simulate_integrate(stores: &Stores, bundle_ids: &[&str]) -> IntegrationResult { ... }

    #[test]
    fn test_stale_rejection_then_rebase_and_repropose() {
        let (tmp, repo) = setup_repo();
        let mgr = setup_worktree_manager(&repo);

        // Session 1: implement work A
        let sha_a = simulate_implement(&mgr, "wk-a", &[("database.py", "# crud functions")]);
        // Session 2: implement work B (concurrent)
        let sha_b = simulate_implement(&mgr, "wk-b", &[("main.py", "# api routes")]);

        // Integrate B first (A becomes stale)
        simulate_integrate(&stores, &["bd-b"]);

        // Reject A for stale base tick
        reset_work_after_bundle_rejection(&stores, "wk-a", "stale base tick");

        // Verify: agent/wk-a branch is rebased onto new integration HEAD
        let rebased_head = get_branch_head(&repo, "agent/wk-a");
        let integ_head = get_branch_head(&repo, "integration/pl-test");
        assert!(is_ancestor(&repo, &integ_head, &rebased_head));

        // Session 3: implementer finds committed code, proposes normal bundle
        let worktree = mgr.get_or_create_branch("wk-a", &integ_head);
        // Should NOT be a noop - branch differs from integration HEAD
        let diff = git_diff(&worktree, &integ_head);
        assert!(!diff.is_empty(), "branch should have real diff after rebase");
    }

    #[test]
    fn test_false_noop_rejected() {
        let (tmp, repo) = setup_repo();
        let mgr = setup_worktree_manager(&repo);

        // Session 1: implement, commit
        simulate_implement(&mgr, "wk-a", &[("database.py", "# crud")]);
        // Cleanup worktree but keep branch
        mgr.cleanup("wk-a");

        // Session 2: reuse branch, try noop
        let worktree = mgr.get_or_create_branch("wk-a", "HEAD");
        let result = handle_propose_bundle(
            &ctx, &worktree, Some("wk-a"),
            "already done", &[], Some("code exists"), &["database.py"],
        ).await;

        // Should be rejected - branch has commits that differ
        assert!(matches!(result, Ok(ActionResult::ActionError(_))));
    }
}
```

**Why this works:** These tests exercise the exact state sequences that caused the E2E failures, in seconds instead of minutes. Each scenario is isolated and deterministic. When a new failure mode is discovered in E2E, it gets added as a test scenario here FIRST, then fixed.

### How the Fixes Interact

The four fixes form a defense-in-depth chain. Here's the full corrected flow for the failure scenario from today's E2E run:

```
1. Implementer A commits CRUD to agent/wk-84cy3 -> bundle bd-1kmar
2. Integrator rejects (stale base tick)
   [Fix 2] -> rebases agent/wk-84cy3 onto new integration HEAD
   -> work reset to Ready

3. Implementer B gets worktree (reuses rebased agent/wk-84cy3)
   -> reads database.py -> CRUD code present (rebased onto current HEAD)
   -> "nothing to commit" -> proposes noop
   [Fix 1] -> detects divergence (agent branch differs from LOOPR_BASE_REF)
   -> auto-converts to normal bundle (branch_name="agent/wk-84cy3", head_commit=<SHA>)

4. Reviewer gets NORMAL bundle
   -> runs git diff HEAD agent/wk-84cy3 -> sees real CRUD diff
   -> approves

5. Integrator merges -> Work -> Done
```

If the implementer crashes before proposing any bundle (Gemini's failure mode):
```
1. Session 1 crashes (missing doc, etc.) -> Failed
   [Fix 3] -> session_failure_count = 1
2. Session 2 crashes -> session_failure_count = 2
3. Session 3 crashes -> session_failure_count = 3 >= max
   [Fix 3] -> Work -> Blocked (no infinite loop)
```

If all fixes work correctly, the test harness (Fix 4) validates each scenario deterministically.

### Architecture

No new components. All fixes are modifications to existing code paths:

```
Implementer -> [Fix 1: false noop guard] -> ProposeBundle
Integrator  -> [Fix 2: rebase on rejection] -> reset_work_after_bundle_rejection
Executor    -> [Fix 3: session failure counter] -> lifecycle terminal handling
Tests       -> [Fix 4: handoff test harness] -> src/tests/integration/handoff.rs
```

### Data Model

One new field, backward compatible:

| Change | Type | Location |
|--------|------|----------|
| `session_failure_count: u32` | New field (serde default=0) | `Work` struct |
| `max-session-failures: u32` | New config (default=3) | `strategy` section in `loopr.yml` |
| `LOOPR_BASE_REF` | Env var passed to implementer | Set by executor |

### Implementation Plan

**Phase 1: Test harness (Fix 4)**
- Create `src/tests/integration/handoff.rs` with repo/worktree helpers
- Write test scenarios 1-8
- All tests should FAIL initially (they exercise the bugs)

**Phase 2: Eliminate false noops (Fix 1)**
- Add divergence check in `handle_propose_bundle`
- Pass base_ref from executor to implementer environment
- Tests 1, 4, 5 should now pass

**Phase 3: Rebase after rejection (Fix 2)**
- Add rebase logic to `reset_work_after_bundle_rejection`
- Handle rebase failure (conflict -> delete branch)
- Tests 2, 3, 8 should now pass

**Phase 4: Session failure counter (Fix 3)**
- Add `session_failure_count` to Work
- Increment in lifecycle terminal handling
- Gate in reconciler
- Test 6 should now pass

**Phase 5: Validation**
- All 8 handoff tests pass
- `otto ci` passes
- Run E2E python-api to validate end-to-end

## Alternatives Considered

### Alternative 1: Delete agent branch on every rejection
- **Description:** After any bundle rejection, delete `agent/<work_id>` branch. Next session always starts clean.
- **Pros:** Eliminates branch reuse divergence entirely. Simplest fix.
- **Cons:** Loses real work. An implementer that spent 15 iterations writing CRUD code has that work destroyed. The next session must redo everything from scratch.
- **Why not chosen:** Rebase (Fix 2) preserves the work when possible, deletes only on conflict. Deleting always is wasteful.

### Alternative 2: Reviewer reads from agent branch instead of integration HEAD
- **Description:** For noop bundles, change `load_bundle_hierarchy` to do `git show <branch>:<path>` instead of `std::fs::read_to_string(repo_path.join(path))`.
- **Pros:** Reviewer sees exactly what the implementer sees. Eliminates context divergence for noop bundles.
- **Cons:** For TRUE noops (scaffold files, no agent branch), there's no branch to read from. Requires special-casing. Also doesn't fix the fundamental problem: the noop bundle is wrong. If the branch has commits, it should be a normal bundle with a diff.
- **Why not chosen:** Fix 1 (eliminate false noops) is more correct. If the branch has real work, it should be proposed as a normal bundle. True noops (no divergence) still read from repo_path correctly.

### Alternative 3: Make the integrator replay stale bundles instead of rejecting
- **Description:** When a bundle has a stale base_tick_id, the integrator rebases the bundle's branch and re-merges instead of rejecting.
- **Pros:** No round-trip through Ready -> InProgress -> InReview. Fastest path.
- **Cons:** `StalePolicy::AutoReplayAndVerify` already exists in the config. It was not used in this E2E run (ReplanAtSafePoint was active). Enabling it requires validation that the replay produces correct results.
- **Why not chosen:** AutoReplay is orthogonal to this design. It could complement Fixes 1-2 but is a separate decision. Fix 2 (rebase on rejection) gives the same benefit at the branch level without requiring integrator-level replay logic.

### Alternative 4: Skip integration tests, add more E2E targets
- **Description:** Instead of unit/integration tests, add more E2E targets that exercise specific failure modes.
- **Pros:** Tests the real system end-to-end.
- **Cons:** Each E2E run takes 10-20 minutes. Finding a bug requires reading logs. Reproducing a specific state sequence is non-deterministic. This is the current approach and it's not working - six rounds, six new bugs.
- **Why not chosen:** E2E tests are valuable for validation but terrible for development. The handoff test harness (Fix 4) provides fast, deterministic feedback for the specific failure modes. E2E remains the final validation step.

## Technical Considerations

### Dependencies

- No new crate dependencies
- One new field on Work (backward compatible via serde default)
- One new config field (backward compatible with default)

### Performance

- Fix 1: One `git diff --quiet` per noop bundle proposal (fast, exits early)
- Fix 2: One `git rebase` per stale rejection (same cost as manual rebase)
- Fix 3: One atomic increment per failed session (negligible)
- Fix 4: Test-only code, no runtime impact

### Security

No security implications. All git operations are local to the target repo.

### Testing Strategy

The test harness (Fix 4) IS the testing strategy. Eight scenarios covering the known failure modes. Each fix is validated by the corresponding test scenario passing.

E2E validation (python-api target) as a final gate after all fixes are implemented.

### Rollout Plan

All changes are internal to the daemon. No config migration needed. `session_failure_count` defaults to 0 for existing records. `max-session-failures` defaults to 3 if not specified in config.

## Edge Cases

### Fix 2 rebase while worktree is checked out
Cannot happen. The sequence is: implementer finishes -> worktree cleaned up -> bundle enters InReview -> reviewer reviews -> integrator rejects -> rebase. By the time the integrator calls `reset_work_after_bundle_rejection`, the implementer's worktree is already removed. The work is InReview (not Ready), so no new implementer session can start until after the reset.

### Fix 2 rebase should hold the git lock
The integrator already uses `self.ctx.stores.lock_git()` for git operations during merge. The rebase in Fix 2 must also acquire the git lock to prevent concurrent git operations (e.g., another integrator cycle or implementer checkout). Add `let _git_guard = self.ctx.stores.lock_git()?;` before the rebase.

### LOOPR_BASE_REF for first-ever session
When a branch doesn't exist yet and the executor creates it from the integration HEAD, LOOPR_BASE_REF equals the integration HEAD SHA. `git diff --quiet <base_ref> HEAD` returns 0 (identical), so the noop proceeds normally. No false trigger.

### Rebase succeeds but code is semantically wrong
If the old integration HEAD had schema A and the new HEAD has schema B, the rebased code may compile but be wrong. This is acceptable - the reviewer or validation commands catch semantic errors. The rebase preserves the opportunity for review; it doesn't guarantee correctness.

### Fix 1 auto-convert for bundles with no new commits
After auto-convert, the normal bundle path runs `git rev-parse HEAD` in the worktree (checked out on the agent branch). This correctly captures the branch HEAD, even though no new commits were made in this session. The auto-commit guard (lines 62-100) runs but finds nothing to commit. The bundle gets branch_name and head_commit from the existing branch state.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Rebase introduces merge conflicts that the implementer can't resolve | Medium | Low | Fix 2 deletes the branch on rebase failure; next session starts clean |
| False noop guard triggers on true noops | Low | Low | Only triggers when `git diff --quiet` shows differences; true noops have no divergence |
| session_failure_count never resets (permanent Blocked) | Low | Medium | Reset to 0 on successful session completion; only consecutive failures count |
| Test harness doesn't cover a real failure mode | Medium | Medium | Each E2E failure gets added as a test scenario before fixing |

## Open Questions

- [ ] Should `StalePolicy::AutoReplayAndVerify` be the default instead of `ReplanAtSafePoint`? Auto-replay would eliminate the stale rejection round-trip entirely. Orthogonal to this design but worth evaluating.
- [x] ~~Should the false-noop guard use the integration branch ref or the base_tick's SHA?~~ Resolved: uses `LOOPR_BASE_REF` (integration branch HEAD SHA at worktree creation time), set by executor as env var.
- [ ] Should the test harness live in `src/tests/integration/handoff.rs` or in a separate test crate? Leaning toward `src/tests/integration/` for consistency with existing test layout.

## References

- Today's E2E run data: `/tmp/loopr/e2e/python-api/latest` (2026-04-10 run, python-api)
- Prior noop design doc: `docs/design/2026-04-10-noop-bundle-lifecycle.md` (Implemented, partially effective)
- Gemini architectural diagnosis (2026-04-10, separate E2E run)
- Reactive execution model: `docs/design/2026-04-09-reactive-execution-model.md`
- Integration branch model: `docs/design/2026-04-09-integration-branch-and-reactive-conflict-resolution.md`
- Worktree manager: `src/worktree/manager.rs`
- Bundle creation: `src/agents/executor/action/bundle.rs`
- Reviewer context: `src/agents/context.rs:414-471`
- Integrator rejection: `src/agents/integrator.rs:320-380, 1446-1484`
- Doom loop safety nets: `project-doom-loop-fix.md` (memory)

---

## Post-Implementation Review: Structural Flaws Found in v0.1.115

An architectural review of the v0.1.115 implementation identified two structural bugs introduced
by Fix 2, plus a test coverage gap. Status updated to "Remediation Pending."

### Flaw 1 (Critical): Double-Lock Deadlock in Merge Failure Path

**Root cause:** Fix 2 added `lock_git()` inside `reset_work_after_bundle_rejection`. However,
the merge-failure path in `process_tick` already holds `_git_guard` (acquired at line 584) when
it calls `reset_work_after_bundle_rejection` at lines 709 and 1411.

`std::sync::Mutex` is non-reentrant. The inner `lock_git()` at line 1502 does not return `Err`
on re-entrancy - it blocks forever. The `match` fallback only fires on mutex poison.

**Affected call sites (called while `_git_guard` is held):**
- `integrator.rs:709` - retryable merge conflict path
- `integrator.rs:1411` - inside `combine_conflicting_works` (also called within `_git_guard` scope)

**Call sites NOT affected (no outer git lock held):**
- `integrator.rs:347` - stale rejection, RejectIfStale
- `integrator.rs:374` - stale rejection, ReplanAtSafePoint
- `integrator.rs:417` - auto-replay failed
- `integrator.rs:994` - validation failure (inner `_git_guard` at line 933 drops before line 994)

### Flaw 2 (Moderate): Read Lock Held Across IPC Loop

Two call sites hold a named `RwLockReadGuard<bundles>` for the full duration of a `for` loop
that contains IPC calls and (after Fix 2) git subprocess spawns. Any concurrent bundle writer
is blocked for the entire loop duration.

**Affected:**
- `integrator.rs:703-711` - retryable merge conflict loop (also contains Flaw 1)
- `integrator.rs:1406-1413` - unrelated-works reset in `combine_conflicting_works` (also Flaw 1)

**NOT affected** (chained temporaries; guard drops at end of `let wi_id = ...;` statement):
- Lines 340-346, 363-369, 987-993

### Flaw 3 (Gap): Test Harness Does Not Exercise Application Code Paths

Tests 2 and 3 in `handoff.rs` verify git mechanics (rebase preserves commits, conflict deletes
branch) by calling git directly. They do not invoke `reset_work_after_bundle_rejection` or
traverse the IPC bridge, which is why the deadlock in Flaw 1 passed undetected.

The git-level tests are valid as boundary condition proofs and should be retained. The gap is
the absence of tests that exercise the Rust code paths through the dispatch harness.

---

## Remediation Plan

### Commit 1: Hoist Rebase + Fix Read-Lock Loops

**Change 1 - Extract `rebase_agent_branch`:**

Remove the git rebase block from `reset_work_after_bundle_rejection`. Extract it into a new
private method `rebase_agent_branch(work_id: &str)` that performs the rebase without acquiring
`lock_git()`. The caller is responsible for ensuring the git lock is either already held (merge
failure path) or explicitly acquired before the call (stale/validation failure paths).

`reset_work_after_bundle_rejection` after extraction: IPC only (work.transition + learning.create).

**Change 2 - Collect-then-iterate at lines 703-711:**

```rust
// Before (guard held across loop)
if let Ok(bundles) = self.ctx.stores.read_bundles() {
    for bundle_id in &valid_bundle_ids {
        let wi_id = bundles.get(...).map(...).unwrap_or_default();
        self.reset_work_after_bundle_rejection(&wi_id, "merge conflict");
    }
}

// After (guard dropped before loop)
let work_ids: Vec<String> = {
    let bundles = self.ctx.stores.read_bundles()?;
    valid_bundle_ids.iter()
        .filter_map(|bid| bundles.get(bid.as_str()).map(|b| b.work_id.clone()))
        .collect()
};
for wi_id in &work_ids {
    self.reset_work_after_bundle_rejection(&wi_id, "merge conflict");
    self.rebase_agent_branch(&wi_id);  // git lock already held by outer _git_guard
}
```

**Change 3 - Collect-then-iterate at lines 1406-1413:**

Same pattern: collect unrelated work IDs into a `Vec<String>` first, drop the bundles guard,
then iterate to call `reset_work_after_bundle_rejection` and `rebase_agent_branch`.

**Change 4 - Non-loop callers (347, 374, 417, 994):**

After each `reset_work_after_bundle_rejection` call, acquire git lock and call
`rebase_agent_branch`:

```rust
self.reset_work_after_bundle_rejection(&wi_id, "stale base tick");
if let Ok(_guard) = self.ctx.stores.lock_git() {
    self.rebase_agent_branch(&wi_id);
}
```

### Commit 2: Add Dispatch Harness Test

Add a test in `src/tests/integration/handoff.rs` (or a new file in `src/tests/integration/`)
that uses the existing dispatch harness (`dispatch_ok`, `inject_preformed_plan`) to:

1. Create a plan/work via `inject_preformed_plan`
2. Transition the work through the bundle rejection lifecycle via `dispatch_ok`
3. Call the work.transition IPC path that `reset_work_after_bundle_rejection` exercises
4. Verify the work state is correct post-reset and that no deadlock occurred

The test does not need to drive a real `IntegratorAgent::process_tick` - it needs to prove that
the state transitions and (separately) the git operations complete without contention under the
new split structure.

### Implementation Order

1. Extract `rebase_agent_branch` - no behavior change, just extraction
2. Remove `lock_git()` from `reset_work_after_bundle_rejection` (now safe, no git ops remain)
3. Refactor loops at 703 and 1406 to collect-then-iterate, wiring in `rebase_agent_branch`
4. Wire `rebase_agent_branch` into non-loop callers with explicit lock acquisition
5. `otto ci` to verify no compile errors or test regressions
6. Add dispatch harness test (Commit 2)
7. `otto ci` again
