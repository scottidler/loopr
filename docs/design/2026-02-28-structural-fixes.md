# Design Document: Loopr v3 MVP6 — 12 Structural Fixes

**Author:** Scott Idler
**Date:** 2026-02-28
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

MVP6 fixes 12 structural flaws found through a manual end-to-end code audit of the post-MVP5 codebase. Flaws range from hard blockers (ProposeBundle always rejected after the first Tick) to logic bugs (phase completion predicate mismatch) to design gaps (no merge conflict resolution). The fixes are organized into 4 dependency-ordered implementation phases: bundle lifecycle safety, Coordinator FSM consistency, dependency resolution, and convergence/observability. Together they unblock loopr from completing a multi-phase build.

## Problem Statement

### Background

MVPs 1–5 built the full orchestration pipeline: FSMs, TaskStore persistence, streaming LLM agents, Integrator, Coordinator FSM with phase-gated control loop, dependency-aware generation, duplicate detection, and convergence controls. The architecture is sound and the plumbing works — Bundles flow through proposal→triage→review→accept→merge→publish, and the Coordinator sequences phases.

### Problem

A manual trace of every critical code path — ProposeBundle through Merge, Coordinator FSM transitions, dependency resolution, retry enforcement — exposed 12 flaws. Three are hard blockers that prevent any multi-Tick build from succeeding. The remaining nine are logic bugs, enforcement gaps, and design limitations that degrade reliability and observability.

**Flaw #1 — ProposeBundle never sends `base_tick_id` (CRITICAL)**

`executor.rs:530-539` builds `bundle.create` params with `work_id`, `branch_name`, `claims`, `description` — but never includes `base_tick_id`. The staleness guard at `handlers.rs:1413-1429` rejects any bundle with `base_tick_id = None` when a Published Tick exists (error -32002). After the first Tick publishes, every subsequent bundle is dead on arrival. Phase 1 might produce one Tick; Phases 2+ never produce any.

```rust
// executor.rs:530-539 — base_tick_id is missing
let resp = bridge.request(
    "bundle.create",
    serde_json::json!({
        "work_id": wi_id,
        "branch_name": branch_name,
        "claims": claims,
        "description": description,
        // BUG: no base_tick_id field
    }),
);
```

**Flaw #2 — Batch dependency resolution (`batch:0`) not implemented (CRITICAL)**

The `generation-work.pmt` prompt teaches the LLM to use `"dependencies": ["batch:0"]` for intra-batch ordering. No code resolves these references. `handlers.rs:1143-1163` logs a warning and silently drops any `batch:*` dependency. When the Coordinator generates 5 Works in one iteration with `batch:0 → batch:1 → batch:2` chains, all dependency edges are lost.

```rust
// handlers.rs:1150-1155 — batch refs silently dropped
if dep_id.starts_with("batch:") {
    log::warn!(
        "Work creation: batch dependency '{}' cannot be resolved at handler level, skipping",
        dep_id
    );
}
```

**Flaw #3 — Git merge runs in main repo with no cleanup on failure (CRITICAL)**

`integrator.rs:651-679` runs `git merge --no-ff <branch>` directly in `stores.config.project.repo_path`. If the merge fails (conflicts, interruption), the repo is left in a half-merged state. No `git merge --abort` cleanup exists anywhere in the codebase (confirmed: zero hits for `merge.*abort` or `reset.*merge`). Subsequent Integrator cycles see a dirty repo and fail repeatedly, bricking the entire pipeline.

```rust
// integrator.rs:660-668 — failure returns Err but never cleans up
if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    return Err(eyre!("git merge {} failed: {}", branch, stderr));
    // BUG: repo left in half-merged state
}
```

**Flaw #4 — `max_work_retries` tracked but never enforced (HIGH)**

`CoordinatorState.work_attempts` exists. `increment_attempts()` exists. `CoordinatorConfig.max_work_retries` defaults to 3. But `increment_attempts()` is never called outside tests — zero production call sites. A single Work can be assigned to implementers infinitely, burning tokens forever. This was the #1 contributor to the 4h53m todo-app run (804 sessions, 90% failure rate).

**Flaw #5 — Executing state prompt missing per-WI dependency info (HIGH)**

`build_phase_status()` at `coordinator.rs:490-527` shows status counts and retry attempts, but not which Works have unmet dependencies. The Coordinator LLM must blindly try `AssignAgent` and discover `DependencyNotMet` errors reactively. With 5+ WIs in a dependency chain, this burns multiple iterations just discovering the execution order.

**Flaw #6 — `is_phase_complete()` ignores Abandoned WIs — mismatch with FSM (HIGH)**

`generation.rs:425-429` checks `all(|w| w.status == Done)` — requires every WI to be Done. But `check_fsm_transition()` at `coordinator.rs:584-592` checks `all(|w| matches!(w.status, Done | Abandoned))` — allows Abandoned. If a WI is Abandoned (max retries, unresolvable), `is_phase_complete()` returns false while the FSM advances past it. The mismatch creates confusing logs and potentially feeds wrong info to the LLM.

```rust
// generation.rs:425-429 — only checks Done
!phase_wis.is_empty() && phase_wis.iter().all(|w| w.status == WorkStatus::Done)

// coordinator.rs:584-592 — allows Done | Abandoned
wis.iter().all(|w| matches!(w.status, WorkStatus::Done | WorkStatus::Abandoned))
```

**Flaw #7 — No worktree refresh / retry on stale bundle rejection (MEDIUM)**

When `ProposeBundle` fails with -32002, `executor.rs:540-542` propagates a generic error. No automatic worktree refresh + re-propose loop. The implementer must re-discover it needs to propose, re-commit, and re-propose in a subsequent iteration. If `max_iterations` is nearly exhausted, it won't recover. The `worktree_mgr.refresh()` method exists but is never called.

**Flaw #8 — `determine_generation_level()` returns None when Draft exists (MEDIUM)**

When a Plan/Spec/Phase is Draft (awaiting validation), `determine_generation_level()` at `generation.rs:281-293` returns `None` because it detects the Draft and stops. The Planning state's `build_generation_footer()` returns `None`, giving the Coordinator no generation instructions. The Coordinator needs to be explicitly directed to validate the existing Draft rather than idling.

**Flaw #9 — Coordinator doesn't surface failure Learnings in phase status (MEDIUM)**

When implementers fail, `executor.rs:229-273` creates a Learning record. The general context builder at `context.rs:495-517` includes Learnings via `select_learnings()` with a 0.6 confidence threshold, but `build_phase_status()` doesn't include phase-specific failure learnings. Critical failure insights may be truncated by the general budget (1500 tokens) before reaching the Coordinator.

**Flaw #10 — No protection against concurrent Integrator + Implementer git operations (MEDIUM)**

The Integrator merges in the main repo. Implementers work in worktrees sharing the same `.git` directory. Concurrent `git merge` in main + `git commit` in worktrees can race on the shared `.git/index.lock` or refs. No locking mechanism prevents this.

**Flaw #11 — No merge conflict resolution (LOW)**

`merge_bundle_branches()` aborts the entire Tick on any merge conflict. No automatic resolution strategy (`--ours`/`--theirs`) exists. When two implementers modify adjacent code, the Tick fails, bundles are Rejected, and both WIs restart from scratch.

**Flaw #12 — Phase record not marked Complete atomically (LOW)**

`complete_phase()` at `coordinator_state.rs:77-85` updates `CoordinatorState.phases_completed` but does NOT transition the Phase record's `status` field (in `stores.phases`) to `HierarchyStatus::Complete`. The Phase record remains Active even after all its WIs are Done and the FSM has moved past it. On crash recovery, the stale status can cause incorrect state reconstruction.

### Flaw Relationship Map

The 12 flaws cluster into 5 dependency groups:

**Bundle lifecycle cluster (#1, #3, #7):** All relate to the proposal→merge→publish pipeline. #1 prevents bundles from being created. #3 can brick the pipeline if a merge fails. #7 loses bundles that could succeed after refresh.

**Coordinator awareness cluster (#5, #8, #9):** All relate to what the Coordinator LLM can see in its prompt context. #5 means it assigns blindly. #8 means it idles during validation. #9 means it can't learn from failures.

**FSM consistency cluster (#6, #12):** Both are mismatches between FSM transition logic and domain record status tracking. #6 is a predicate mismatch; #12 is a timing/atomicity issue.

**Convergence cluster (#4, #2):** #4 (no retry enforcement) and #2 (dropped dependencies) together cause runaway loops. Without retry limits, failed WIs retry forever. Without dependency ordering, WIs execute in wrong order, increasing failure rates.

**Git safety cluster (#3, #10, #11):** All relate to unsafe git operations. #3 is the most dangerous (fixed in Phase 1). #10 is a race condition. #11 is a design limitation.

### Goals

1. All 3 CRITICAL flaws fixed — multi-Tick builds can complete
2. All 3 HIGH flaws fixed — enforcement gaps closed
3. All 4 MEDIUM flaws fixed — degraded behavior eliminated
4. LOW flaws addressed (2 fixed, merge conflict resolution design-documented for future)

### Non-Goals

- Changing the Implementer, Reviewer, or Integrator agent prompts (their pipelines work)
- Changing the FSM transition rules (they're correct, just inconsistently applied)
- Changing the TaskStore persistence model
- Automatic merge conflict resolution (documented as future work)
- Parallel phase execution (sequential phases are correct for now)

## Proposed Solution

### Overview

Twelve fixes organized into 4 dependency-ordered implementation phases:

| Phase | Focus | Flaws | Severity Fixed |
|-------|-------|-------|----------------|
| 1 | Bundle Lifecycle & Merge Safety | #1, #3, #7 | 2 CRITICAL + 1 MEDIUM |
| 2 | Coordinator FSM Consistency | #6, #8, #12 | 1 HIGH + 1 MEDIUM + 1 LOW |
| 3 | Dependency Resolution | #2, #5 | 1 CRITICAL + 1 HIGH |
| 4 | Convergence & Observability | #4, #9, #10, #11 | 1 HIGH + 2 MEDIUM + 1 LOW |

### Architecture

#### Phase 1: Bundle Lifecycle & Merge Safety

##### Fix #1 — Include `base_tick_id` in ProposeBundle

The executor's `ProposeBundle` action handler must query Stores for the latest Published Tick and include its ID in `bundle.create` params.

```rust
// executor.rs — ProposeBundle action handler
AgentAction::ProposeBundle { work_id, branch_name, claims, description } => {
    let wi_id = work_id.ok_or_else(|| eyre!("propose_bundle requires work_id"))?;

    // Resolve base_tick_id from latest Published Tick
    let base_tick_id = {
        let ticks = stores.ticks.read().unwrap();
        ticks.values()
            .filter(|t| t.status == TickStatus::Published)
            .max_by_key(|t| t.number)
            .map(|t| t.id.clone())
    };

    let mut params = serde_json::json!({
        "work_id": wi_id,
        "branch_name": branch_name,
        "claims": claims,
        "description": description,
    });
    if let Some(tick_id) = base_tick_id {
        params["base_tick_id"] = serde_json::Value::String(tick_id);
    }

    let resp = bridge.request("bundle.create", params);
    // ...
}
```

##### Fix #3 — Add `git merge --abort` cleanup on failure

Wrap the merge in `merge_bundle_branches()` with cleanup on failure:

```rust
// integrator.rs — merge_bundle_branches()
fn merge_bundle_branches(repo_path: &std::path::Path, bundle_branches: &[String]) -> Result<String> {
    for branch in bundle_branches {
        let output = std::process::Command::new("git")
            .args(["merge", "--no-ff", branch, "-m", &format!("Merge bundle branch {}", branch)])
            .current_dir(repo_path)
            .output()
            .map_err(|e| eyre!("git merge {} failed to execute: {}", branch, e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Clean up the half-merged state
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

##### Fix #7 — Stale bundle retry with worktree refresh

On -32002 error from `bundle.create`, refresh the worktree to the latest Tick and re-attempt once:

```rust
// executor.rs — ProposeBundle error handling
if resp.is_error() {
    if let Some(ref err) = resp.error {
        if err.code == -32002 {
            // Stale bundle — refresh worktree and retry once
            log::warn!("Bundle stale ({}), refreshing worktree and retrying", err.message);
            if let Some(ref wt_path) = worktree_path {
                worktree_mgr.refresh(wt_path)?;
            }
            // Re-resolve base_tick_id
            let new_base_tick_id = resolve_latest_published_tick_id(stores);
            params["base_tick_id"] = new_base_tick_id.into();
            let retry_resp = bridge.request("bundle.create", params);
            if retry_resp.is_error() {
                return Err(eyre!("propose bundle failed after retry: {:?}", retry_resp.error));
            }
            return Ok(ActionResult::BundleProposed { /* ... */ });
        }
    }
    return Err(eyre!("propose bundle failed: {:?}", resp.error));
}
```

#### Phase 2: Coordinator FSM Consistency

##### Fix #6 — Align `is_phase_complete()` with FSM terminal status check

```rust
// generation.rs — is_phase_complete()
pub fn is_phase_complete(stores: &Stores, phase_id: &str) -> bool {
    let works = stores.works.read().unwrap();
    let phase_wis: Vec<_> = works.values().filter(|w| w.phase_id == phase_id).collect();
    !phase_wis.is_empty()
        && phase_wis.iter().all(|w| {
            matches!(w.status, WorkStatus::Done | WorkStatus::Abandoned)
        })
}
```

This matches the FSM's `check_fsm_transition()` predicate exactly. Both now treat Done and Abandoned as terminal.

##### Fix #8 — Direct Coordinator to validate Draft documents in Planning state

When `determine_generation_level()` returns `None` because a Draft exists, inject explicit validation instructions instead of letting the Coordinator idle:

```rust
// coordinator.rs — handle_planning()
fn handle_planning(stores: &Stores, /* ... */) -> CoordinatorAction {
    match determine_generation_level(stores) {
        Some(level) => {
            // Existing generation logic
            build_generation_footer(stores, level)
        }
        None => {
            // Check if a Draft document exists that needs validation
            let draft_context = find_pending_draft(stores);
            if let Some(draft) = draft_context {
                // Inject "validate this Draft" instruction
                format!(
                    "A {} is in Draft status and needs validation. \
                     Use ValidateDocument to validate it before proceeding.\n\
                     Draft ID: {}\nTitle: {}",
                    draft.level, draft.id, draft.title
                )
            } else {
                // All documents Active — transition to ActivatePhase
                // (handled by FSM transition check)
                String::new()
            }
        }
    }
}
```

##### Fix #12 — Mark Phase record Complete atomically in PhaseGate

```rust
// coordinator.rs — handle_phase_gate()
fn handle_phase_gate(stores: &Stores, coord_state: &mut CoordinatorState) {
    if let Some(ref phase_id) = coord_state.current_phase_id {
        // Transition the Phase domain record to Complete
        let mut phases = stores.phases.write().unwrap();
        if let Some(phase) = phases.get_mut(phase_id) {
            phase.status = HierarchyStatus::Complete;
            phase.updated_at = id::now_millis();
        }
    }
    // Then update CoordinatorState
    coord_state.complete_phase();
}
```

The Phase record status and CoordinatorState are now updated together in the same handler, before the FSM transitions to ActivatePhase.

#### Phase 3: Dependency Resolution

##### Fix #2 — Implement batch dependency resolution

The executor maintains a `batch_created_ids: Vec<String>` across action executions within a single iteration. After creating each Work via `CreateWork`, it appends the new ID. When processing dependencies, `batch:N` references resolve to `batch_created_ids[N]`.

```rust
// executor.rs — run iteration with batch tracking
async fn run_iteration(/* ... */) -> Result<Vec<ActionResult>> {
    let actions = parse_llm_response(response)?;
    let mut batch_created_ids: Vec<String> = Vec::new();
    let mut results = Vec::new();

    for action in actions {
        match action {
            AgentAction::CreateWork { dependencies, .. } => {
                // Resolve batch: references before sending to daemon
                let resolved_deps: Vec<String> = dependencies.iter().map(|dep| {
                    if let Some(idx_str) = dep.strip_prefix("batch:") {
                        if let Ok(idx) = idx_str.parse::<usize>() {
                            if let Some(resolved_id) = batch_created_ids.get(idx) {
                                return resolved_id.clone();
                            }
                            log::warn!("batch:{} out of range (only {} items created so far)", idx, batch_created_ids.len());
                        }
                        dep.clone() // fall through — handler will drop unknown refs
                    } else {
                        dep.clone()
                    }
                }).collect();

                let result = create_work(bridge, /* ..., */ &resolved_deps).await?;
                if let ActionResult::WorkCreated { id, .. } = &result {
                    batch_created_ids.push(id.clone());
                }
                results.push(result);
            }
            _ => {
                results.push(execute_action(action, /* ... */).await?);
            }
        }
    }
    Ok(results)
}
```

##### Fix #5 — Include per-WI dependency info in Executing state prompt

Enhance `build_phase_status()` to show each Work's dependencies and their satisfaction status:

```rust
// coordinator.rs — build_phase_status() addition
for wi in &phase_wis {
    if !wi.dependencies.is_empty() {
        let dep_status: Vec<String> = wi.dependencies.iter().map(|dep_id| {
            let status = works.get(dep_id)
                .map(|d| format!("{}", d.status))
                .unwrap_or_else(|| "unknown".to_string());
            format!("{}={}", dep_id, status)
        }).collect();
        let all_met = wi.dependencies.iter().all(|dep_id| {
            works.get(dep_id)
                .map(|d| d.status == WorkStatus::Done)
                .unwrap_or(false)
        });
        summary.push_str(&format!(
            "  [{}] {} — deps: [{}] ({})\n",
            wi.id,
            wi.title,
            dep_status.join(", "),
            if all_met { "READY" } else { "BLOCKED" }
        ));
    }
}
```

The Coordinator can now proactively assign only WIs whose dependencies are all met, instead of discovering unmet deps through `DependencyNotMet` errors.

#### Phase 4: Convergence & Observability

##### Fix #4 — Wire `increment_attempts()` into production code

Call `increment_attempts()` each time an implementer is assigned to a Work, and enforce the limit:

```rust
// coordinator.rs — handle_executing(), after AssignAgent action
AgentAction::AssignAgent { agent_type: "implementer", target_id } => {
    let attempts = coord_state.increment_attempts(&target_id);
    let max_retries = config.coordinator.max_work_retries;

    if attempts > max_retries {
        log::warn!(
            "Work {} exceeded max retries ({}/{}), transitioning to Abandoned",
            target_id, attempts, max_retries
        );
        transition_work(stores, &target_id, WorkStatus::Abandoned)?;
        // Create Learning about the repeated failure
        create_retry_exhaustion_learning(stores, &target_id, attempts)?;
        continue; // skip assignment
    }

    // Proceed with AssignAgent
    match bridge.request("agent.start", params).await {
        Ok(_) => { /* success */ }
        Err(e) if e.code == -32011 => {
            // DependencyNotMet — skip, don't count as attempt
            coord_state.decrement_attempts(&target_id);
        }
        Err(e) => return Err(e.into()),
    }
}
```

##### Fix #9 — Surface failure Learnings in `build_phase_status()`

Add phase-specific failure learnings to the Coordinator's phase status context:

```rust
// coordinator.rs — build_phase_status() addition
let learnings = stores.learnings.read().unwrap();
let phase_failures: Vec<_> = learnings.values()
    .filter(|l| {
        l.scope == "phase"
            && phase_wis.iter().any(|wi| l.source_id == wi.id)
    })
    .collect();

if !phase_failures.is_empty() {
    summary.push_str("\nRecent failure learnings:\n");
    for learning in phase_failures.iter().take(5) {
        summary.push_str(&format!("  - {}\n", learning.content));
    }
}
```

This ensures the Coordinator sees failure insights prominently — not buried in the general context builder's budget-limited Learnings section.

##### Fix #10 — Advisory lock for git operations on main repo

Add a `Mutex` guard around git operations that touch the main repo:

```rust
// Add to Stores or a shared context
pub struct GitLock {
    main_repo: tokio::sync::Mutex<()>,
}

// integrator.rs — acquire lock before merge
let _guard = git_lock.main_repo.lock().await;
let sha = merge_bundle_branches(repo_path, &branches)?;
drop(_guard);
```

Implementer worktree operations (commit, push) don't need the lock since `git worktree` isolates the working tree. The lock only protects ref-mutating operations on the main checkout (merge, reset, checkout).

##### Fix #11 — Merge conflict resolution (deferred)

Merge conflict auto-resolution is out of scope for MVP6. The current behavior (abort entire Tick on any conflict) is preserved. The mitigations are:

1. Sequential phase execution reduces cross-implementer conflicts
2. Dependency ordering within phases serializes related changes
3. Fix #3 ensures failed merges are cleaned up safely

Future work: add a configurable conflict resolution strategy (`abort`, `ours`, `theirs`, `manual`) to the Integrator. The `manual` option would pause the Tick and emit a `need_help` event for human intervention.

### Data Model

No new domain types. Changes to existing types:

**`CoordinatorState`** — no structural changes. The `increment_attempts()` method is now called in production (Fix #4).

**`Phase`** — `status` field now transitioned to `HierarchyStatus::Complete` atomically in PhaseGate (Fix #12). No field changes.

**`Stores`** — add `git_lock: Arc<GitLock>` field for advisory locking (Fix #10).

### Config Changes

No new config fields. Existing `max_work_retries`, `phase_timeout_secs`, `goal_timeout_secs` are now enforced (Fix #4).

### API Changes

No new IPC methods. Behavioral changes:

| Method | Change |
|--------|--------|
| `bundle.create` | Now receives `base_tick_id` from executor (Fix #1) |

No new ActionResult variants needed — all existing variants (`DependencyNotMet`, `DuplicateDetected`, etc.) were defined in MVP5.

## Implementation Plan

### Phase 1: Bundle Lifecycle & Merge Safety

**Files:** `src/agents/executor.rs`, `src/agents/integrator.rs`

1. Add `resolve_latest_published_tick_id()` helper that returns `Option<String>` for the latest Published Tick's ID
2. Include `base_tick_id` in ProposeBundle's `bundle.create` params
3. Add `git merge --abort` cleanup in `merge_bundle_branches()` on merge failure
4. Add stale-bundle retry path: on -32002, call `worktree_mgr.refresh()`, re-resolve `base_tick_id`, retry once
5. Test: ProposeBundle includes correct `base_tick_id` when Published Tick exists
6. Test: ProposeBundle omits `base_tick_id` when no Published Tick (bootstrap case)
7. Test: merge failure cleans up — repo is not left in half-merged state
8. Test: stale bundle retry succeeds after worktree refresh

### Phase 2: Coordinator FSM Consistency

**Files:** `src/agents/generation.rs`, `src/agents/coordinator.rs`, `src/domain/coordinator_state.rs`

1. Align `is_phase_complete()` to check `Done | Abandoned` (match FSM predicate)
2. Add `find_pending_draft()` helper to detect Draft Plan/Spec/Phase
3. Inject "validate Draft" instruction in Planning state when `determine_generation_level()` returns None and a Draft exists
4. Move Phase record status transition into `handle_phase_gate()` — update `stores.phases` to `Complete` before calling `complete_phase()`
5. Test: `is_phase_complete()` returns true when all WIs are Done or Abandoned
6. Test: Planning state emits validation instruction when Draft Plan exists
7. Test: Phase record status is Complete after PhaseGate processes it

### Phase 3: Dependency Resolution

**Files:** `src/agents/executor.rs`, `src/agents/coordinator.rs`

1. Add `batch_created_ids: Vec<String>` tracking to executor iteration loop
2. Resolve `batch:N` references to real IDs before sending `work.create` to daemon
3. Enhance `build_phase_status()` with per-WI dependency list and satisfaction status (READY/BLOCKED)
4. Test: batch dependency `batch:0` resolves to first created Work's ID
5. Test: out-of-range batch index falls through gracefully (warning, not error)
6. Test: `build_phase_status()` output includes dependency info

### Phase 4: Convergence & Observability

**Files:** `src/agents/coordinator.rs`, `src/agents/integrator.rs`, `src/daemon/context.rs`

1. Wire `increment_attempts()` into `handle_executing()` on each implementer assignment
2. Enforce `max_work_retries` — transition to Abandoned when exceeded
3. Add `decrement_attempts()` for `DependencyNotMet` (don't count blocked attempts)
4. Add phase-specific failure Learnings to `build_phase_status()` output
5. Add `GitLock` with `tokio::sync::Mutex` around main repo git operations
6. Test: attempts increment on each assignment, WI transitions to Abandoned at limit
7. Test: DependencyNotMet does not count as an attempt
8. Test: `build_phase_status()` includes failure learnings
9. Test: concurrent Integrator merges are serialized by GitLock

## Alternatives Considered

### Alternative 1: Fix Only the 3 CRITICAL Flaws

- **Description:** Ship fixes #1, #2, #3 and defer everything else.
- **Pros:** Smallest change set. Multi-Tick builds would technically work.
- **Cons:** Without retry enforcement (#4), the system still runs away on failures. Without FSM consistency (#6, #12), crash recovery is unreliable. Without dependency visibility (#5), the Coordinator wastes iterations on blind assignments. The HIGH/MEDIUM flaws collectively degrade the system enough that real-world builds would still fail frequently.
- **Why not chosen:** The HIGH flaws are almost as impactful as the CRITICAL ones. Fixing only CRITICAL would produce a system that technically starts but practically fails.

### Alternative 2: Merge Conflict Auto-Resolution Now

- **Description:** Add `--ours` or `--theirs` merge strategy to the Integrator.
- **Pros:** Would handle conflicts automatically. Fewer Tick rejections.
- **Cons:** Both `--ours` and `--theirs` silently discard code. Without understanding which side is correct, auto-resolution produces broken builds. A human-in-the-loop strategy is safer but requires UI work (not in scope). The frequency of conflicts is low after phase gating and dependency ordering are in place (Fixes #2, #5, #6).
- **Why not chosen:** Deferred. The risk/reward ratio is unfavorable until we observe conflict frequency with all other fixes in place.

### Alternative 3: Replace Advisory Lock with Git Worktree for Integrator

- **Description:** Run the Integrator in its own worktree instead of the main checkout.
- **Pros:** Eliminates the main-repo contention entirely. No lock needed.
- **Cons:** The Integrator needs to merge branches and update refs on the main repo — a worktree doesn't isolate ref operations. The merged result must end up on main. Using a worktree would add complexity (merge in worktree, then fast-forward main) without eliminating the ref race.
- **Why not chosen:** Advisory lock is simpler and sufficient. The Integrator's 10s cycle means contention is rare.

## Technical Considerations

### Dependencies

- **Internal:** TaskStore Record trait (existing), FSM infrastructure (existing), IPC framework (existing)
- **External:** None new. All changes use existing dependencies.

### Performance

- **`resolve_latest_published_tick_id()`:** O(t) scan of Ticks collection (typically 1-10). Called once per ProposeBundle.
- **`git merge --abort` on failure:** Adds one git command on the error path only. No performance impact on success.
- **`batch_created_ids` tracking:** Vec append per CreateWork. Negligible.
- **Per-WI dependency info in prompt:** O(w*d) where w is Works in phase (3-10) and d is deps per WI (0-3). Negligible.
- **`GitLock` contention:** Integrator runs every 10s, lock held for merge duration (~1-5s). Implementer worktree operations don't need the lock. Contention is rare.

### Testing Strategy

Each phase includes unit tests. Key scenarios:

1. **ProposeBundle base_tick_id:** Correct Tick ID included after first Tick publishes; omitted at bootstrap
2. **Merge cleanup:** Failed merge leaves repo in clean state (not half-merged)
3. **Stale bundle retry:** Refresh + retry produces successful bundle proposal
4. **FSM predicate alignment:** `is_phase_complete()` matches `check_fsm_transition()` for all terminal status combinations
5. **Draft validation:** Planning state emits validation instruction for Draft documents
6. **Phase record atomicity:** Phase status is Complete after PhaseGate, survives simulated crash
7. **Batch dependency resolution:** `batch:0` → real ID; out-of-range → graceful fallback
8. **Dependency visibility:** `build_phase_status()` output includes READY/BLOCKED markers
9. **Retry enforcement:** 3 failures → Abandoned; DependencyNotMet doesn't count
10. **Failure learnings in prompt:** Phase-specific failure learnings appear in Coordinator context
11. **Git advisory lock:** Concurrent merge attempts are serialized

### Rollout Plan

1. **Phase 1** (bundle + merge safety) — deploy independently, immediate unblock for multi-Tick builds
2. **Phase 2** (FSM consistency) — deploy independently, improves crash recovery and Planning state
3. **Phase 3** (dependency resolution) — deploy after Phase 2 (depends on consistent FSM for phase gating)
4. **Phase 4** (convergence + observability) — deploy independently, prevents runaway loops

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Stale bundle retry creates duplicate bundles | Low | Medium | Handler already rejects duplicate bundles (same work_id + branch). Retry is idempotent. |
| `git merge --abort` fails (no merge in progress) | Low | Low | `--abort` is a no-op when no merge is in progress. Return code ignored via `let _ =`. |
| Batch dependency index off-by-one | Medium | Medium | Log warning and drop unresolvable ref. Phase gating still provides cross-phase ordering. |
| `is_phase_complete()` change causes premature phase advancement | Low | High | Only broadens acceptance (adds Abandoned). Phase with all-Abandoned WIs advances — this is correct, the phase is terminal. PhaseGate handler logs the outcome. |
| Advisory lock deadlock | Low | High | Lock is held only during `merge_bundle_branches()` (bounded duration). Single acquirer pattern (Integrator). Tokio mutex is fair. |
| `increment_attempts()` double-counts on retry | Medium | Medium | `decrement_attempts()` on DependencyNotMet. Only count actual implementer spawns. |
| Phase-specific learnings spam the prompt | Low | Medium | Limited to 5 most recent via `.take(5)`. Coordinator can ignore if not relevant. |

## Open Questions

- [ ] **Should stale bundle retry be configurable (max retries)?** Recommendation: hardcode 1 retry for MVP6. A single refresh+retry handles the common case (new Tick published between worktree creation and bundle proposal). Multiple retries suggest a deeper problem.
- [ ] **Should `decrement_attempts()` exist or should DependencyNotMet skip the increment entirely?** Recommendation: skip the increment. Cleaner than increment-then-decrement. Check dependencies before calling `increment_attempts()`.
- [ ] **Should Fix #10 (GitLock) be deferred?** The race condition requires precise timing (Integrator merge + implementer worktree ref operation overlapping). With the Integrator's 10s cycle and sub-second lock duration, contention is unlikely. Could defer to a future robustness pass.

## References

- [MVP1 Design Doc](2026-02-25-orchestration-spine.md) — orchestration spine
- [MVP2 Design Doc](2026-02-26-taskstore-doc-validator.md) — TaskStore + Doc Validator
- [MVP3 Design Doc](2026-02-26-implementer-reviewer-agents.md) — Implementer + Reviewer agents
- [MVP4 Design Doc](2026-02-26-multi-level-rwl.md) — multi-level RWL, Coordinator, Integrator
- [MVP5 Design Doc](2026-02-28-coordinator-sequencing.md) — Coordinator control loop & sequential dependencies
- [E2E Blockers](2026-02-27-e2e-blockers.md) — pipeline integration fixes
- [Audit Fixes](2026-02-27-audit-fixes.md) — 23 defects found and fixed
