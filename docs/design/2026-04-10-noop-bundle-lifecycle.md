# Design Document: Noop Bundle Lifecycle

**Author:** Scott A. Idler
**Date:** 2026-04-10
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Noop bundles - where the implementer claims a Work item is already satisfied without code changes - cause a three-part stall that deadlocks the pipeline. The reviewer gets no file contents to verify the claim, the integrator skips the integration branch checkout creating a crash-window for `integration_sha`, and the system has no recovery path once stuck. This design addresses all three failure modes to make noop bundles a first-class citizen of the tick lifecycle.

## Problem Statement

### Background

Noop bundles were introduced so implementers can signal "this work is already done" when files pre-exist in the scaffold or on the integration branch. The executor sets `noop_reason` on the bundle, `branch_name` to empty string, and `paths` to empty vec. The integrator skips the merge for noop bundles, and the reviewer gets a special "verify current state" directive.

### Problem

During the python-api E2E run (2026-04-09), Work `wk-yuqvw` ("requirements.txt - pinned dependency file") stalled permanently because:

1. **Reviewer context starvation**: The implementer proposed a noop bundle with `paths: []`. The context builder used those empty paths to read files for the reviewer, producing nothing. The reviewer rejected ("no file contents were provided for review"). This repeated 3+ times.

2. **Crash-window for `integration_sha`**: For noop ticks, the `if !branches.is_empty()` guard at `integrator.rs:583` skips the entire merge block - including integration branch checkout and the pre-publish SHA capture at line 619. The `integration_sha` is only set *after* the tick transitions to Published (line 793), creating a TOCTOU window. If the daemon crashes between the publish transition (line 788, which writes to JSONL) and the SHA update (line 803), the JSONL contains a Published tick with `integration_sha: None`. On restart, `audit_tick_shas` flags this as CATASTROPHIC and enters DEGRADED mode, halting all tick creation.

3. **No recovery path**: Once a bundle reaches Merged status, the coordinator's override guard (`executor/action/work.rs:274-296`) blocks resetting the Work to Ready ("Cannot override Work: bundle is Merged"). The reconciler doesn't handle InReview + all-bundles-terminal. The system is permanently stuck.

### Goals

- Noop bundles provide the reviewer with file contents to verify the noop claim
- `integration_sha` is always set before the tick transitions to Published (for ALL tick types)
- Noop ticks checkout the integration branch for correct validation and SHA recording
- The system recovers gracefully when all bundles for a work are noop

### Non-Goals

- Eliminating noop bundles entirely (they serve a legitimate purpose)
- Changing the decomposer to detect pre-existing files (valuable but separate concern)
- Modifying the scaffold to exclude files that will be assigned as Work items (also separate)

## Proposed Solution

### Overview

Four surgical fixes, each addressing one link in the failure chain:

1. **Populate noop bundle paths** via explicit `noop_paths` on ProposeBundle
2. **Checkout integration branch for noop ticks** before validation
3. **Set `integration_sha` before publish transition** (belt-and-suspenders for all tick types)
4. **Add coordinator sweep** for InReview works with all bundles terminal

### Current vs Fixed Flow

**Current noop flow (broken)**:
```
Implementer: file exists -> ProposeBundle(noop_reason="already exists", paths=[])
Reviewer:    gets empty paths -> reads no files -> rejects ("no evidence")
             ... repeats 3+ times ...
Integrator:  branches=[] -> skips checkout+merge -> integration_sha=None
             tick transitions to Published -> JSONL has Published+no SHA
             daemon restarts -> audit finds no SHA -> CATASTROPHIC -> DEGRADED
Coordinator: tries override -> blocked by Merged bundle guard -> STUCK
```

**Fixed noop flow**:
```
Implementer: file exists -> ProposeBundle(noop_reason="...", noop_paths=["requirements.txt"])
Executor:    validates paths exist -> stores on bundle.paths
Reviewer:    gets paths -> reads file contents -> verifies against AC -> approves
Integrator:  branches=[] -> checkouts integration branch -> records HEAD as integration_sha
             tick transitions to Published -> JSONL has Published+SHA -> clean
Coordinator: sweep_integrated_to_done -> Work advances to Done
```

### Fix 1: Explicit `noop_paths` on ProposeBundle

**Location**: `src/agents/action.rs:83-90` and `src/agents/executor/action/bundle.rs:121-139`

**Current behavior**: For noop bundles, `paths` is hardcoded to `vec![]`. The `ProposeBundle` action has no way for the implementer to specify which files it verified.

**New behavior**: Add a `noop_paths` field to the `ProposeBundle` action. When proposing a noop bundle, the implementer must list the files it inspected.

```rust
// src/agents/action.rs
ProposeBundle {
    #[serde(default, alias = "summary")]
    description: String,
    #[serde(default, deserialize_with = "string_or_vec")]
    claims: Vec<String>,
    #[serde(default)]
    noop_reason: Option<String>,
    #[serde(default)]
    noop_paths: Option<Vec<String>>,  // Required when noop_reason is set
},
```

The executor validates:
- If `noop_reason` is set and `noop_paths` is empty/None, fall back to extracting file-like tokens from the Work's `title` and `acceptance_criteria`. A file-like token is any string containing a dot or slash that resolves to an existing file in the repo.
- All paths are validated to exist in the repo (relative to repo root). Non-existent paths are filtered out with a warning.
- **If no valid paths remain after filtering, the ProposeBundle action MUST fail** by returning `ActionResult::ActionError("Noop bundle requires at least one valid file path. Provide noop_paths listing the files you verified.")`. This prevents creating a dead-on-arrival bundle that would trap the reviewer in a rejection loop - the exact failure mode this design exists to fix.

```rust
// src/agents/executor/action/bundle.rs
let paths: Vec<String> = if !is_noop {
    // Existing: git diff --name-only main...HEAD
    ...
} else {
    // Use explicit noop_paths from the implementer, or fall back to Work context
    let candidate_paths = noop_paths.unwrap_or_else(|| extract_paths_from_work(bridge, &wi_id));
    // Validate: keep only paths that exist in the repo
    candidate_paths.into_iter()
        .filter(|p| worktree_path.join(p).exists())
        .collect()
};
```

The `extract_paths_from_work` fallback:
- Reads the Work's title and acceptance_criteria
- Extracts tokens that look like file paths (contain `.` or `/`)
- Returns candidates for validation

The reviewer's context builder at `context.rs:441-448` works correctly without modification - it already reads files from the paths list. With non-empty paths, the reviewer gets actual file contents to verify.

**Prompt update**: The implementer system prompt must instruct the LLM to include `noop_paths` when proposing a noop bundle: "List every file you read or verified that supports the noop claim."

### Fix 2: Integration Branch Checkout for Noop Ticks

**Location**: `src/agents/integrator.rs:583`

**Current behavior**: The block `if !branches.is_empty() && is_git_repo` wraps both the integration branch checkout AND the merge. When all bundles are noop, branches is empty, and the entire block is skipped. This means:
- Validation commands run on whatever branch is currently checked out (wrong state)
- `get_git_head_sha()` returns the wrong SHA
- `integration_sha` is None when the tick transitions to Published

**New behavior**: Separate the integration branch checkout from the merge. The git guard must span both checkout and merge to prevent concurrent git operations.

```rust
if is_git_repo {
    let _git_guard = self.ctx.stores.lock_git()?;

    // Always checkout integration branch (needed for validation + SHA)
    if let Some(ref branch) = integ_branch {
        let verify = std::process::Command::new("git")
            .args(["rev-parse", "--verify", branch])
            .current_dir(repo_path)
            .output();
        if let Ok(o) = verify && o.status.success() {
            let checkout = std::process::Command::new("git")
                .args(["checkout", branch])
                .current_dir(repo_path)
                .output();
            if let Ok(co) = checkout && !co.status.success() {
                let stderr = String::from_utf8_lossy(&co.stderr);
                return Err(eyre!("failed to checkout integration branch {}: {}", branch, stderr));
            }
        } else {
            return Err(eyre!(
                "integration branch {} does not exist (deleted or not yet created)",
                branch
            ));
        }
    }

    pre_merge_sha = get_git_head_sha(repo_path);

    if !branches.is_empty() {
        // Merge bundle branches (existing code)
        match merge_bundle_branches(repo_path, &branches) {
            Ok(sha) => {
                let mut ticks = self.ctx.stores.write_ticks()?;
                if let Some(tick) = ticks.get_mut(&tick_id) {
                    tick.integration_sha = Some(sha);
                }
            }
            Err(e) => { /* existing rollback logic */ }
        }
    } else {
        // Noop tick: no merge needed, but record integration branch HEAD
        self.ctx.info("All bundles are noop - skipping merge, recording HEAD SHA");
        let sha = get_git_head_sha(repo_path);
        let tick_to_persist = {
            let mut ticks = self.ctx.stores.write_ticks()?;
            if let Some(tick) = ticks.get_mut(&tick_id) {
                tick.integration_sha = sha;
                Some(tick.clone())
            } else {
                None
            }
        };
        // Persist immediately so crash between here and publish doesn't lose SHA
        if let Some(tick) = tick_to_persist
            && let Some(ref store) = self.ctx.stores.store
            && let Ok(mut s) = store.lock().map_err(|_| eyre!("taskstore lock poisoned"))
        {
            let _ = s.update(tick);
        }
    }
}
```

This ensures:
- Validation commands run on the integration branch (correct state)
- `get_git_head_sha()` returns the integration branch HEAD (correct SHA)
- `integration_sha` is set AND persisted before publish for noop ticks
- The git guard spans the entire checkout+merge sequence, preventing races

### Fix 3: Set integration_sha Before Publish Transition

**Location**: `src/agents/integrator.rs:774-806`

**Current behavior**: The `tick.transition` handler (`daemon/handlers/tick.rs:232-238`) persists the entire tick object to JSONL when transitioning to Published. For normal ticks, `integration_sha` was set at line 619 during the merge, so it's already populated. For noop ticks, `integration_sha` is None at this point - it's only set at line 793 (post-publish) and persisted at line 803. If the daemon crashes between the transition write and the SHA update, JSONL contains a Published tick with no SHA.

**New behavior**: As a belt-and-suspenders defense, ensure `integration_sha` is set in memory before the publish transition. Fix 2 already handles the noop case by setting SHA after checkout. This fix adds a defensive check for ALL tick types:

```rust
// 11. Publish or Fail
if passed {
    // Defensive: ensure integration_sha is set before publish transition.
    // Normal ticks already have it from merge (line 619). Noop ticks have
    // it from Fix 2 checkout. This catches any remaining gap.
    //
    // IMPORTANT: Fetch SHA outside the write lock to avoid blocking the
    // daemon's state access with a synchronous subprocess spawn.
    let missing_sha = {
        let ticks = self.ctx.stores.read_ticks()?;
        ticks.get(&tick_id).map_or(false, |t| t.integration_sha.is_none())
    };

    if missing_sha {
        let sha = get_git_head_sha(repo_path)
            .unwrap_or_else(|| "unknown".to_string());
        self.ctx.warn(&format!(
            "integration_sha was None before publish - setting to {} (defensive)",
            sha
        ));
        let mut ticks = self.ctx.stores.write_ticks()?;
        if let Some(tick) = ticks.get_mut(&tick_id) {
            tick.integration_sha = Some(sha);
        }
    }

    // NOW transition to Published - the handler persists the tick with SHA populated
    let pub_resp = self.ctx.bridge.request(
        "tick.transition",
        serde_json::json!({
            "id": tick_id,
            "target_status": "Published",
            "role": "integrator",
        }),
    );
    // ... existing error handling ...
}
```

The post-publish SHA set (current lines 790-806) becomes redundant and can be removed. The single JSONL write from the transition handler now contains the SHA.

**Why this works**: The `tick.transition` handler reads the in-memory tick (which now has `integration_sha` set), applies `force_status(Published)`, clones, and writes to JSONL. A single atomic write, no TOCTOU window.

### Fix 4: Coordinator Sweep for Stuck InReview Works

**Location**: `src/agents/coordinator.rs` (alongside `sweep_integrated_to_done`)

**Current behavior**: The coordinator's `sweep_integrated_to_done` function transitions Integrated works to Done on each iteration. But there is no equivalent sweep for InReview works where the integrator failed to advance them.

**Why not in the reconciler**: The reconciler (`reconcile.rs`) takes only `&Stores` - it has no IPC bridge access. The InReview -> Integrated transition requires `role: Integrator` per the FSM. The coordinator run loop has bridge access and can issue proper IPC-based transitions.

**New behavior**: Add a `sweep_stuck_inreview` function alongside `sweep_integrated_to_done` in the coordinator run loop. It runs on every Executing iteration:

```rust
fn sweep_stuck_inreview(
    stores: &Stores,
    coord_state: &CoordinatorState,
    bridge: &AgentIpcBridge,
    prefix: &str,
) {
    if coord_state.fsm_state != CoordinatorFsmState::Executing {
        return;
    }

    let stuck: Vec<String> = {
        let Ok(works) = stores.read_works() else { return };
        let Ok(bundles) = stores.read_bundles() else { return };

        works.values()
            .filter(|w| w.status() == WorkStatus::InReview)
            .filter(|w| {
                let work_bundles: Vec<_> = bundles.values()
                    .filter(|b| b.work_id == w.id)
                    .collect();
                // All bundles must be terminal, and at least one must be Merged
                !work_bundles.is_empty()
                    && work_bundles.iter().all(|b| b.status().is_terminal())
                    && work_bundles.iter().any(|b| b.status() == BundleStatus::Merged)
            })
            .map(|w| w.id.clone())
            .collect()
    };

    for wi_id in &stuck {
        tracing::warn!(
            "{} sweep_stuck_inreview: Work {} is InReview with all bundles terminal \
             (at least one Merged) - advancing to Integrated",
            prefix, wi_id
        );
        let resp = bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id,
                "target_status": "Integrated",
                "role": "integrator",  // FSM requires Integrator role
            }),
        );
        if resp.is_error() {
            tracing::error!("{} sweep_stuck_inreview: failed to advance {}", prefix, wi_id);
        }
    }
}
```

Called from `run_iteration` right after `sweep_integrated_to_done`:

```rust
sweep_integrated_to_done(stores, coord_state, bridge, &prefix);
sweep_stuck_inreview(stores, coord_state, bridge, &prefix);
```

This is a safety net. If Fixes 1-3 work correctly, this sweep should never find stuck works. But it prevents permanent stalls from edge cases we haven't anticipated.

### Data Model

One additive schema change. No breaking changes.

- `ProposeBundle.noop_paths: Option<Vec<String>>` - **new field** on the action enum (serde default = None, backward compatible)
- `Bundle.paths: Vec<String>` - populated instead of empty for noop bundles (existing field, no schema change)
- `Tick.integration_sha: Option<String>` - set before publish instead of after (existing field, no schema change)
- `Work.status` - transitioned by coordinator sweep when stuck (existing field, no schema change)

### Implementation Plan

**Phase 1: Critical path (Fixes 2+3)** - Integrator
- Separate integration branch checkout from merge in integrator
- Set integration_sha before publish transition
- Add tests for noop tick lifecycle

**Phase 2: Reviewer context (Fix 1)** - Executor + Action Schema
- Add `noop_paths` to ProposeBundle action
- Fallback: parse Work title/AC for file paths
- Update implementer prompt to instruct noop_paths usage
- Add tests for noop bundle path population

**Phase 3: Safety net (Fix 4)** - Coordinator
- Add `sweep_stuck_inreview` function alongside `sweep_integrated_to_done`
- Add tests for stuck work recovery

## Alternatives Considered

### Alternative 1: Skip review for noop bundles entirely
- **Description:** Fast-track noop bundles from Proposed directly to Accepted, bypassing review.
- **Pros:** Eliminates reviewer context starvation entirely. Simpler flow.
- **Cons:** Implementers might incorrectly claim noop when files are wrong or incomplete. No verification of the noop claim. Violates the safety principle that all code changes (or claims of no-change) are reviewed.
- **Why not chosen:** Reviewer verification of noop claims catches real errors (e.g., file exists but doesn't meet AC). The review is valuable; it just needs proper context.

### Alternative 2: Have the implementer inline file contents in noop_reason
- **Description:** The implementer pastes the file contents into the `noop_reason` string field.
- **Pros:** No executor changes needed. Reviewer gets contents via the noop_reason text.
- **Cons:** Bloats the noop_reason field. Mixes metadata with content. Duplicates the file content reading that the context builder already does.
- **Why not chosen:** The existing `paths` + context builder pipeline is the right place for this. Just needs non-empty paths.

### Alternative 3: Decomposer detects pre-existing files and avoids creating noop work
- **Description:** Before creating a Work item, the decomposer checks if the target file already exists and meets AC.
- **Pros:** Eliminates noop bundles at the source.
- **Cons:** Requires the decomposer to evaluate AC (complex). Doesn't handle cases where files appear after decomposition (e.g., created by an earlier Work item). Doesn't fix the integrator bugs for legitimate noop cases.
- **Why not chosen:** Good complementary fix but doesn't address the integrator's structural issues. Should be a separate design doc.

### Alternative 4: Set integration_sha in the tick.transition RPC handler
- **Description:** The `tick.transition` handler sets `integration_sha` to current HEAD when transitioning to Published.
- **Pros:** Atomic with the transition. No TOCTOU window.
- **Cons:** The handler doesn't know which branch should be checked out. It would capture whatever HEAD is at call time, which may be wrong if the integration branch isn't checked out.
- **Why not chosen:** Fix 2 (checkout integration branch first) + Fix 3 (set SHA before transition) achieves the same atomicity without coupling the handler to git state.

## Technical Considerations

### Dependencies

- No new crate dependencies
- No changes to TaskStore, IPC protocol, or domain model schemas
- One additive field on the `AgentAction::ProposeBundle` enum variant (backward compatible via `serde(default)`)

### Performance

- Fix 1: Path validation against filesystem (one stat per path, negligible)
- Fix 2: One additional `git checkout` for noop ticks (same cost as current non-noop path)
- Fix 3: No additional write - SHA is set in memory before the existing publish write
- Fix 4: One additional scan of bundles per coordinator iteration (same pattern as sweep_integrated_to_done)

### Testing Strategy

**Unit tests** (in `src/agents/integrator/tests.rs`):
- `test_noop_tick_sets_integration_sha` - verify SHA is set after noop tick cycle
- `test_noop_tick_checkouts_integration_branch` - verify correct branch is checked out
- `test_integration_sha_set_before_publish` - verify SHA is in JSONL before transition

**Unit tests** (in `src/agents/executor/action/tests.rs`):
- `test_noop_bundle_uses_explicit_paths` - verify noop_paths are stored on the bundle
- `test_noop_bundle_fallback_paths_from_work` - verify fallback path extraction from Work title/AC
- `test_noop_bundle_filters_nonexistent_paths` - verify invalid paths are filtered out

**Unit tests** (in `src/agents/coordinator/tests.rs`):
- `test_sweep_stuck_inreview_with_merged_bundle` - verify escape hatch fires
- `test_sweep_inreview_not_all_terminal` - verify escape hatch doesn't fire prematurely
- `test_sweep_inreview_no_merged_bundle` - verify escape hatch doesn't fire when all bundles are Rejected

**E2E test**:
- Run python-api E2E with a scaffold that includes `requirements.txt` (the exact failure case)
- Verify the work advances through noop to Done without stalling

## Edge Cases

### Mixed tick (noop + non-noop bundles)
The `branches` collection already filters out noop bundles (empty `branch_name`). If at least one bundle is non-noop, `branches` is non-empty and the merge proceeds normally. Noop bundles are skipped during merge but still transition to Merged after publish. Fix 2 handles this correctly - the integration branch checkout always happens, then the merge-or-skip decision follows.

### No integration branch (Brief mode, no plan)
When `integ_branch` is None (no plan_id), the checkout block is skipped. For noop ticks without an integration branch: `get_git_head_sha()` captures whatever is checked out (typically main). This is correct behavior - no integration branch means we're working directly on main.

### Implementer provides wrong noop_paths
If the implementer lists files that exist but don't satisfy the AC, the reviewer sees the actual file contents and rejects. The coordinator retries with a new implementer session. Self-correcting.

### Post-publish SHA code (cleanup)
After Fix 3, the post-publish SHA set at current lines 790-806 becomes a no-op (SHA already set). It should be simplified to a debug assertion: `debug_assert!(tick.integration_sha.is_some(), "SHA should be set before publish")`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Implementer omits noop_paths | Medium | Low | Fallback to Work title/AC parsing; reviewer can still reject with clear reason |
| Integration branch checkout fails for noop tick | Low | Medium | Existing error handling returns Err, tick stays Sealing, retried next cycle |
| Coordinator sweep fires incorrectly | Low | High | Only fires when ALL bundles are terminal AND at least one is Merged; logged as warning for audit; uses proper FSM transition with Integrator role |
| Extra JSONL write adds latency | Low | Low | Single small write; already on the persistence path |

## Open Questions

- [ ] Should the decomposer avoid creating Work items for files that already exist in the scaffold? (Separate design doc candidate)
- [x] ~~Should `audit_tick_shas` tolerate `integration_sha: None` for noop ticks?~~ No - Fixes 2+3 ensure SHA is always set. Keep the strict invariant.
- [x] ~~Should noop bundles skip validation commands?~~ No - validation is harmless (tests existing state) and maintains the invariant that all Published ticks are validated. After Fix 2, validation runs on the correct branch.

## References

- E2E run data: `/tmp/loopr/e2e/python-api/latest` (2026-04-09 run)
- Integrator source: `src/agents/integrator.rs`
- Bundle creation: `src/agents/executor/action/bundle.rs`
- Reviewer context: `src/agents/context.rs:415-471, 699-732`
- Override guard: `src/agents/executor/action/work.rs:274-296`
- Reconciler: `src/agents/coordinator/reconcile.rs`
- Integration branch model: `docs/design/` (integration branch design doc)
