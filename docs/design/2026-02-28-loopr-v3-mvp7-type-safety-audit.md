# Design Document: Loopr v3 MVP7 — IPC Type Safety & Lifecycle Audit Fixes

**Author:** Scott Idler
**Date:** 2026-02-28
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

MVP7 fixes 18 issues found through an exhaustive audit of every IPC sender→handler pair, every domain struct↔action enum mapping, and every FSM lifecycle path in the post-MVP6 codebase. The bugs range from two critical blockers (Work lifecycle dead-ends at InReview, Coordinator can't see merged Bundles) to five silent data-loss mismatches (claims dropped, learnings lose scope/roles/tags) to logic bugs in the Integrator merge-failure path and retry enforcement. The root cause across most findings is the same: the executor serializes typed Rust structs to untyped `serde_json::json!({})` params, the handler deserializes them with ad-hoc `v.as_str()` / `from_value()` calls, and no compile-time contract enforces agreement between the two sides.

## Problem Statement

### Background

MVPs 1–6 built the full orchestration pipeline and fixed 12 structural flaws. The architecture is sound — FSMs, TaskStore persistence, streaming LLM agents, Integrator, Coordinator FSM, phase gating, dependency resolution, retry enforcement, and advisory git locking are all in place. But an exhaustive audit of every IPC call site, every handler, and every FSM lifecycle path exposed a new class of bugs: type mismatches and lifecycle gaps that silently lose data or deadlock the pipeline.

### Problem

The system has a fundamental design weakness: **the executor→daemon IPC boundary has no compile-time type safety**. The executor builds `serde_json::Value` params by hand (`json!({"claims": claims})`), and the handler parses them by hand (`v.as_str().unwrap_or("")`). When the two sides disagree on the type (Vec<String> vs String), the casing convention (`"Phase"` vs `"phase"`), or which fields exist, the mismatch is silent — `as_str()` returns `None` for arrays, `from_value()` returns `Err` for wrong casing, and missing fields default to empty.

This produced 18 confirmed issues across 5 categories:

**Category 1 — Critical lifecycle blockers (2):** The Work lifecycle dead-ends at InReview because nobody executes the InReview→Integrated transition, and the Coordinator can't see merged Bundles to know which WIs need advancing.

**Category 2 — Silent data loss at IPC boundary (5):** ProposeBundle.claims (Vec→String mismatch), learning scope casing ("Phase" vs "phase"), learning applicable_roles (sent but never read), learning resource_tags (sent but never read), worktree.refresh missing work_id.

**Category 3 — Logic bugs (4):** Merge failure orphans Bundles, merge failure bypasses FSM/persistence for Ticks, retry counter burns slots on non-DependencyNotMet errors, mark_phase_record_complete doesn't persist to TaskStore.

**Category 4 — Latent/naming issues (4):** LearningScope lowercase vs AgentAction snake_case convention clash, HierarchyStatus lowercase vs LLM PascalCase expectations, files_changed vs touched_paths param name inconsistency, Reviewer sends title as source_id.

**Category 5 — Dead code (3):** ActionResult variants DuplicateDetected, PhaseCompleted, GoalCompleted are declared but never produced.

### Goals

1. Fix all 2 critical lifecycle blockers — WIs can reach Done after bundle merge
2. Fix all 5 silent data-loss bugs — claims, learning scope/roles/tags, worktree refresh all work
3. Fix all 4 logic bugs — merge failure cleanup, retry enforcement, persistence
4. Introduce typed IPC param structs to prevent future mismatches at compile time
5. Clean up dead code and naming inconsistencies

### Non-Goals

- Rewriting the entire IPC layer (typed structs are introduced incrementally, not all at once)
- Changing FSM transition rules for Work/Bundle/Plan/Spec/Phase (the rules are correct; the issue is who executes them). Exception: one Tick FSM rule is added (`Sealing → Failed`) because it was genuinely missing.
- Changing the LLM prompt format or AgentAction enum variants (only fixing the executor↔handler contract)
- Full integration test suite rewrite (targeted regression tests for each fix)

## Proposed Solution

### Overview

18 fixes organized into 5 dependency-ordered implementation phases:

| Phase | Focus | Issues Fixed | Severity |
|-------|-------|-------------|----------|
| 1 | Work lifecycle & Coordinator visibility | C1, C2 | 2 CRITICAL |
| 2 | IPC data-loss fixes | M1, M2, M3, M4, M5 | 5 DATA LOSS |
| 3 | Integrator merge-failure & retry enforcement | B2, B3, B4 | 3 BUG |
| 4 | Persistence & scoping | L1, L2 | 2 BUG |
| 5 | Typed IPC params, naming cleanup, dead code | M6, M7, M8, M9, M10-12 | 7 LOW/LATENT/DEAD |

### Issue Cross-Reference

Each fix maps to an audit finding. The IDs come from two audits: the lifecycle tracing audit (C/B/L prefix) and the exhaustive mismatch audit (M prefix).

| Fix | Audit ID | Source File(s) | Handler/Target File(s) |
|-----|----------|---------------|----------------------|
| C1 | Lifecycle trace | `integrator.rs:506-521` | `work.rs:62-66` (FSM rule) |
| C2 | Lifecycle trace | `coordinator.rs:128-131` | `coordinator.rs:build_state_summary()` |
| M1 | Mismatch audit | `executor.rs:549` → `handlers.rs:1431-1436` | `bundle.rs:143` (claims field) |
| M2 | Mismatch audit | `coordinator.rs:955` → `handlers.rs:1919-1928` | `learning.rs:17` (LearningScope serde) |
| M3 | Mismatch audit | `executor.rs:611-613` → `handlers.rs:1947` | `learning.rs:53` (applicable_roles) |
| M4 | Mismatch audit | `executor.rs:614-616` → `handlers.rs:1947` | `learning.rs:57` (resource_tags) |
| M5 | Mismatch audit | `integrator.rs:256` → `handlers.rs:2579-2582` | `worktree/manager.rs` |
| B2 | Lifecycle trace | `integrator.rs:411-424` | Bundle status orphaned |
| B3 | Lifecycle trace | `integrator.rs:414-418` | `tick.rs` (missing FSM rule) |
| B4 | Lifecycle trace | `coordinator.rs:934` | `coordinator.rs:976-979` |
| L1 | Lifecycle trace | `coordinator.rs:375-384` | TaskStore persistence |
| L2 | Lifecycle trace | `coordinator.rs:find_pending_draft_for_validation()` | Unscoped query |
| M6 | Mismatch audit | `learning.rs:17` | serde alias gap |
| M7 | Mismatch audit | `plan.rs:13` | serde alias gap |
| M8 | Mismatch audit | `handlers.rs:1448` vs `handlers.rs` (update) | Param name inconsistency |
| M9 | Mismatch audit | Reviewer prompt → `executor.rs` | source_id semantic |
| M10-12 | Mismatch audit | `executor.rs:1160-1172` | Dead ActionResult variants |

### Architecture

#### Phase 1: Work Lifecycle & Coordinator Visibility

##### Fix C1 — Integrator transitions WI InReview → Integrated after bundle merge

**Problem:** `work.rs:62-66` defines `InReview → Integrated` with `Role::Integrator`. The Integrator merges Bundles and transitions them `Integrating → Merged` (integrator.rs:506-521), but never touches the parent Work. The Coordinator can't do it either — the FSM restricts `InReview → Integrated` to `Role::Integrator` only. Result: every successfully-implemented WI gets stuck at InReview forever.

**Fix:** After the Integrator transitions bundles to Merged, it also transitions each bundle's parent Work from InReview → Integrated. This is the natural place — the Integrator already knows which bundles were merged and can look up their `work_id`.

```rust
// integrator.rs — after transitioning bundles to Merged (line ~521)

// C1: Transition parent Works InReview → Integrated
let merged_wi_ids: Vec<String> = {
    let bundles = stores.bundles.read().unwrap();
    valid_bundle_ids.iter()
        .filter_map(|id| bundles.get(id.as_str()))
        .map(|b| b.work_id.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
};

for wi_id in &merged_wi_ids {
    let should_transition = {
        let wis = stores.works.read().unwrap();
        wis.get(wi_id).map(|w| w.status == WorkStatus::InReview).unwrap_or(false)
    };
    if should_transition {
        let resp = bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id,
                "target_status": "Integrated",
                "role": "integrator",
            }),
        );
        if resp.is_error() {
            warn!("Integrator: failed to transition WI {} to Integrated: {:?}", wi_id, resp.error);
        } else {
            info!("Integrator: Work {} transitioned to Integrated", wi_id);
        }
    }
}
```

The Coordinator's Executing footer already says "Transition Integrated Works to Done" — and the Coordinator has `Role::Coordinator` permission for `Integrated → Done` (work.rs:68-70). So the full lifecycle now works: InReview →(Integrator)→ Integrated →(Coordinator)→ Done.

##### Fix C2 — Show recently-merged Bundles in state summary

**Problem:** `build_state_summary()` at coordinator.rs:128-131 filters out `Merged | Rejected | Superseded` bundles. The Coordinator can't see which WIs have merged bundles and need the `Integrated → Done` transition.

**Fix:** Add a separate "Recently Merged" section that shows bundles merged in the current phase, linking them to their parent WI. To avoid unbounded growth, only show merged bundles whose parent WI is NOT yet Done (i.e., bundles the Coordinator still needs to act on).

```rust
// coordinator.rs — build_state_summary(), after the Bundles section

// C2: Show recently-merged bundles whose parent WI still needs advancing
{
    let bundles = stores.bundles.read().unwrap();
    let works = stores.works.read().unwrap();
    let actionable_merged: Vec<_> = bundles.values()
        .filter(|b| b.status == BundleStatus::Merged)
        .filter(|b| {
            // Only show if the parent WI is NOT terminal
            works.get(&b.work_id)
                .map(|w| !matches!(w.status, WorkStatus::Done | WorkStatus::Abandoned))
                .unwrap_or(true)
        })
        .collect();
    if !actionable_merged.is_empty() {
        summary.push_str("### Recently Merged Bundles (WI needs advancing)\n");
        for b in &actionable_merged {
            let wi_status = works.get(&b.work_id)
                .map(|w| w.status.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            summary.push_str(&format!(
                "- [{}] Merged (wi: {} [{}], branch: {})\n",
                b.id, b.work_id, wi_status, b.branch_name
            ));
        }
        summary.push('\n');
    }
}
```

This gives the Coordinator direct visibility into which WIs have completed the merge pipeline and still need the `Integrated → Done` transition. Once a WI reaches Done, its merged bundles are filtered out of the summary.

#### Phase 2: IPC Data-Loss Fixes

##### Fix M1 — Bundle.claims: Vec<String> → String mismatch

**Problem:** `AgentAction::ProposeBundle.claims` is `Vec<String>` (mod.rs:238). The executor sends `"claims": claims` which serializes as a JSON array `["claim1", "claim2"]`. The handler at handlers.rs:1431-1436 reads `v.as_str()` which returns `None` for arrays → defaults to `""`. Bundle.claims is `String` (bundle.rs:143). Every bundle loses its claims.

**Fix:** Two changes — the handler must parse arrays, and the domain model should store the richer type.

```rust
// domain/bundle.rs — change claims from String to Vec<String>
pub struct Bundle {
    // ...
    pub claims: Vec<String>,  // was: String
    // ...
}

impl Bundle {
    pub fn new(work_id: String, base_tick_id: Option<String>, branch_name: String, claims: Vec<String>) -> Self {
        // ...
        Self {
            // ...
            claims,
            // ...
        }
    }
}

// daemon/handlers.rs — handle_bundle_create: parse claims as array
let claims: Vec<String> = match req.params.get("claims") {
    Some(serde_json::Value::Array(arr)) => {
        arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()
    }
    Some(serde_json::Value::String(s)) => vec![s.clone()],  // backward compat
    _ => Vec::new(),
};
```

All callers of `Bundle::new()` must be updated. The `claims` field is used in:
- `handle_bundle_create` (handlers.rs) — updated above
- `handle_bundle_update` (handlers.rs) — update to same pattern
- Test code — update to pass `Vec<String>`
- `build_state_summary` display — join with `, ` for display

##### Fix M2 — Learning scope casing: "Phase" vs "phase"

**Problem:** The Coordinator retry-exhaustion path at coordinator.rs:955 sends `"scope": "Phase"` (PascalCase). `LearningScope` uses `#[serde(rename_all = "lowercase")]` (learning.rs:17), expecting `"phase"`. `from_value()` fails, the handler returns an error, and `let _ =` at coordinator.rs:951 silently ignores it. Learnings from retry exhaustion are never created.

**Fix:** Fix the sender to use the correct casing. Also add `#[serde(alias = "...")]` to LearningScope variants for robustness against future LLM casing mistakes.

```rust
// coordinator.rs — retry exhaustion learning (line ~955)
"scope": "phase",  // was: "Phase"

// domain/learning.rs — add aliases for robustness
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LearningScope {
    #[serde(alias = "Work", alias = "work")]
    Work,
    #[serde(alias = "Phase")]
    Phase,
    #[serde(alias = "Spec")]
    Spec,
    #[serde(alias = "Plan")]
    Plan,
    #[serde(alias = "Global")]
    Global,
}
```

##### Fix M3 — Handler drops applicable_roles

**Problem:** The executor sends `"applicable_roles": [...]` (executor.rs:611-613). `handle_learning_create` (handlers.rs:1908-1966) never reads it. `Learning::new()` defaults `applicable_roles` to `None`.

**Fix:** Parse applicable_roles in the handler and set it on the Learning before persisting.

```rust
// daemon/handlers.rs — handle_learning_create, after creating learning
let mut learning = Learning::new(source_id, scope, content);

// M3: Parse applicable_roles
if let Some(roles_val) = req.params.get("applicable_roles") {
    if let Ok(roles) = serde_json::from_value::<Vec<Role>>(roles_val.clone()) {
        learning.applicable_roles = Some(roles);
    }
}

// M4: Parse resource_tags
if let Some(tags_val) = req.params.get("resource_tags") {
    if let Ok(tags) = serde_json::from_value::<Vec<String>>(tags_val.clone()) {
        learning.resource_tags = tags;
    }
}
```

##### Fix M4 — Handler drops resource_tags

**Problem:** Same pattern as M3. Executor sends `"resource_tags": [...]` (executor.rs:614-616). Handler never reads it. Fixed in the same code block as M3 above.

##### Fix M5 — AutoReplayAndVerify sends empty params to worktree.refresh

**Problem:** Integrator's AutoReplayAndVerify stale policy at integrator.rs:256 sends `bridge.request("worktree.refresh", serde_json::json!({}))`. Handler at handlers.rs:2579-2582 requires `work_id` → returns error. The auto-replay always fails, falling back to rejecting the bundle.

**Fix:** Pass the bundle's `work_id` and the latest published tick's integration SHA. The variable `latest_tick_id` (a `String`) is already in scope from the stale-detection logic at integrator.rs:193. We need to look up the actual Tick to get `integration_sha` for the `new_base_ref`.

```rust
// integrator.rs — AutoReplayAndVerify path (line ~256)
crate::config::StalePolicy::AutoReplayAndVerify => {
    let wi_id = {
        let bundles = stores.bundles.read().unwrap();
        bundles.get(stale_id).map(|b| b.work_id.clone()).unwrap_or_default()
    };
    // latest_tick_id is already in scope from line ~193
    let new_base_ref = {
        let ticks = stores.ticks.read().unwrap();
        ticks.values()
            .find(|t| t.id == latest_tick_id)
            .and_then(|t| t.integration_sha.clone())
            .unwrap_or_else(|| "HEAD".to_string())
    };
    let refresh_resp = bridge.request(
        "worktree.refresh",
        serde_json::json!({
            "work_id": wi_id,
            "new_base_ref": new_base_ref,
        }),
    );
    // ... rest unchanged
}
```

#### Phase 3: Integrator Merge-Failure & Retry Enforcement

##### Fix B2 — Merge failure orphans Bundles in Integrating state

**Problem:** When `merge_bundle_branches()` fails (integrator.rs:411), the code sets `tick.status = Failed` and returns. But the Bundles remain in Integrating state — they're never transitioned to Rejected. The validation-failure path (line 541-557) properly does `Integrating → Rejected` for each bundle, but the merge-failure path doesn't.

**Fix:** Add bundle rejection in the merge-failure path, mirroring the validation-failure path.

```rust
// integrator.rs — merge failure path (line ~411-424)
Err(e) => {
    warn!("Integrator: merge failed: {}", e);
    // Fail the tick via IPC (not direct mutation)
    let fail_resp = bridge.request(
        "tick.transition",
        serde_json::json!({
            "id": tick_id,
            "target_status": "Failed",
            "role": "integrator",
        }),
    );
    if fail_resp.is_error() {
        error!("Integrator: failed to fail tick {}: {:?}", tick_id, fail_resp.error);
    }

    // B2: Transition bundles Integrating → Rejected
    for bundle_id in &valid_bundle_ids {
        let resp = bridge.request(
            "bundle.transition",
            serde_json::json!({
                "id": bundle_id,
                "target_status": "Rejected",
                "role": "integrator",
            }),
        );
        if resp.is_error() {
            warn!("Integrator: failed to reject bundle {} after merge failure: {:?}", bundle_id, resp.error);
        }
    }

    return Ok(IntegratorCycleResult::ValidationFailed {
        tick_id,
        log: format!("Merge failed: {}", e),
    });
}
```

##### Fix B3 — Merge failure bypasses FSM, events, and TaskStore persistence

**Problem:** The merge-failure path at integrator.rs:414-418 sets `tick.status = TickStatus::Failed` directly on the in-memory HashMap, bypassing FSM transition rules, event emission, and TaskStore persistence.

**Root cause:** The tick FSM has no `Sealing → Failed` transition rule. `tick_transitions()` only defines `Sealing → Validating`, `Validating → Published`, and `Validating → Failed`. A merge failure happens during Sealing (before Validating), so there's no valid FSM path to Failed. The original code used direct mutation as a workaround.

**Fix (two parts):**

1. Add `Sealing → Failed` transition rule to `tick_transitions()`:
```rust
// domain/tick.rs — add to tick_transitions()
TransitionRule {
    from: Sealing,
    to: Failed,
    role: Some(Role::Integrator),
},
```

2. Use the IPC bridge to transition the tick (as shown in Fix B2 above). With the new FSM rule, `tick.transition` will validate correctly, emit the `transition.completed` event, and persist to TaskStore.

The current code `tick.status = TickStatus::Failed` is removed entirely — the bridge request in Fix B2 handles it correctly now that the FSM rule exists.

##### Fix B4 — Retry counter burns slots on non-DependencyNotMet errors

**Problem:** `increment_attempts()` at coordinator.rs:934 runs BEFORE `execute_action()`. If `execute_action()` returns `ActionError` (WI already InProgress, WI not found, transition error), the count is incremented but never decremented. Only `DependencyNotMet` gets the decrement at line 976-979. Other `ActionError`s silently burn retry slots.

**Fix:** Move the increment to after successful agent spawn, or extend the decrement to cover all non-spawn results.

```rust
// coordinator.rs — Fix B4: Only count successful spawns as attempts
if let AgentAction::AssignAgent { agent_type, target_id } = action_ref
    && agent_type == "implementer"
{
    let attempts = coord_state.increment_attempts(target_id);
    let max_retries = config.max_work_retries;
    if attempts > max_retries {
        // ... abandon logic unchanged
        continue;
    }

    let result = execute_action(action_ref, tool_runner, bridge, repo_root, None, AgentType::Coordinator).await;
    match result {
        Ok(ActionResult::AgentSpawned { .. }) => {
            // Attempt counts — agent was actually spawned
        }
        Ok(ActionResult::DependencyNotMet { ref work_id, .. }) => {
            // Don't count — dependency not met, not a real attempt
            if let Some(count) = coord_state.work_attempts.get_mut(work_id) {
                *count = count.saturating_sub(1);
            }
        }
        Ok(_) | Err(_) => {
            // Don't count — action failed before agent spawned
            if let Some(count) = coord_state.work_attempts.get_mut(target_id) {
                *count = count.saturating_sub(1);
            }
        }
    }
    // ... handle result
    continue;
}
```

#### Phase 4: Persistence & Scoping

##### Fix L1 — mark_phase_record_complete doesn't persist to TaskStore

**Problem:** `mark_phase_record_complete()` at coordinator.rs:375-384 updates the in-memory Phase record to `HierarchyStatus::Complete` but never calls `store.update()`. On crash/restart, the Phase still shows Active in TaskStore.

**Fix:** Add clone-then-drop-then-persist pattern (consistent with all other persistence sites in the codebase).

```rust
// coordinator.rs — mark_phase_record_complete
fn mark_phase_record_complete(stores: &Stores, coord_state: &CoordinatorState) {
    if let Some(ref phase_id) = coord_state.current_phase_id {
        let phase_to_persist = {
            let mut phases = stores.phases.write().unwrap();
            if let Some(phase) = phases.get_mut(phase_id) {
                phase.status = HierarchyStatus::Complete;
                phase.updated_at = crate::id::now_millis();
                info!("Phase {} marked Complete (record status updated)", phase_id);
                Some(phase.clone())
            } else {
                None
            }
        };
        // L1: Persist to TaskStore
        if let Some(phase) = phase_to_persist
            && let Some(ref store) = stores.store
        {
            if let Err(e) = store.lock().unwrap().update(phase) {
                warn!("Failed to persist Phase complete status: {}", e);
            }
        }
    }
}
```

##### Fix L2 — find_pending_draft_for_validation is unscoped

**Problem:** `find_pending_draft_for_validation()` finds ANY Draft document globally instead of scoping to the current hierarchy chain. An old Draft from a previous abandoned regeneration cycle (not explicitly Abandoned, just leftover Draft) would be found and block progress.

**Fix:** Scope the search to documents that are children of the current Active hierarchy chain.

```rust
// coordinator.rs — find_pending_draft_for_validation
fn find_pending_draft_for_validation(stores: &Stores) -> Option<PendingDraft> {
    // Only look for Drafts that are part of the current active hierarchy
    let active_plan_id = {
        let plans = stores.plans.read().unwrap();
        plans.values().find(|p| p.status == HierarchyStatus::Active).map(|p| p.id.clone())
    };

    // Check for Draft Plan (only if no Active Plan exists)
    if active_plan_id.is_none() {
        let plans = stores.plans.read().unwrap();
        if let Some(draft) = plans.values().find(|p| p.status == HierarchyStatus::Draft) {
            return Some(PendingDraft { level: "Plan", id: draft.id.clone(), title: draft.title.clone() });
        }
        return None;
    }

    let active_spec_id = {
        let specs = stores.specs.read().unwrap();
        specs.values()
            .find(|s| s.status == HierarchyStatus::Active && Some(&s.plan_id) == active_plan_id.as_ref())
            .map(|s| s.id.clone())
    };

    // Check for Draft Spec (only children of the active Plan)
    if active_spec_id.is_none() {
        let specs = stores.specs.read().unwrap();
        if let Some(draft) = specs.values().find(|s| {
            s.status == HierarchyStatus::Draft && Some(&s.plan_id) == active_plan_id.as_ref()
        }) {
            return Some(PendingDraft { level: "Spec", id: draft.id.clone(), title: draft.title.clone() });
        }
        return None;
    }

    // Check for Draft Phase (only children of the active Spec)
    let phases = stores.phases.read().unwrap();
    if let Some(draft) = phases.values().find(|p| {
        p.status == HierarchyStatus::Draft && Some(&p.spec_id) == active_spec_id.as_ref()
    }) {
        return Some(PendingDraft { level: "Phase", id: draft.id.clone(), title: draft.title.clone() });
    }

    None
}
```

#### Phase 5: Typed IPC Params, Naming Cleanup, Dead Code

##### Fix M6 & M7 — Add serde aliases for case-insensitive deserialization

**Problem:** LearningScope uses `rename_all = "lowercase"` but LLMs/executors may send PascalCase or snake_case. HierarchyStatus uses `rename_all = "lowercase"` but LLM Transition actions may send PascalCase.

**Fix:** Add `#[serde(alias)]` on every variant (shown in Fix M2 for LearningScope). For HierarchyStatus:

```rust
// domain/plan.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HierarchyStatus {
    #[serde(alias = "Draft")]
    Draft,
    #[serde(alias = "Active")]
    Active,
    #[serde(alias = "Complete")]
    Complete,
    #[serde(alias = "Abandoned")]
    Abandoned,
}
```

The handler's `from_value()` calls will now accept both `"draft"` and `"Draft"`.

##### Fix M8 — Normalize files_changed / touched_paths param name

**Problem:** `handle_bundle_create` reads `"files_changed"`, `handle_bundle_update` reads `"touched_paths"`. Same underlying field. Confusing for any caller.

**Fix:** Both handlers accept both names, preferring `"touched_paths"` (matches the struct field name).

```rust
// daemon/handlers.rs — normalize in both create and update
let touched_paths_val = req.params.get("touched_paths")
    .or_else(|| req.params.get("files_changed"));
if let Some(files) = touched_paths_val.and_then(|v| v.as_array()) {
    bundle.touched_paths = files.iter().filter_map(|v| v.as_str().map(String::from)).collect();
}
```

##### Fix M9 — Reviewer sends title as source_id

**Problem:** The Reviewer sends `work_title` as the `source_id` for `learning.create`. All other senders use actual record IDs. This breaks source_id-based filtering/joining.

**Fix:** The Reviewer should send the Work ID, not the title. The reviewer prompt's CreateLearning instruction should specify `source_id` is the WI ID. If the Reviewer doesn't have the WI ID in context, the executor should inject it (since it knows `work_id` from the session).

```rust
// agents/executor.rs — CreateLearning handler, inject work_id if source_id looks like a title
// (This is a heuristic; the real fix is the prompt, but defense-in-depth is good)
let effective_source_id = if !source_id.starts_with("wi-") && !source_id.starts_with("phase-")
    && !source_id.starts_with("plan-") && !source_id.starts_with("spec-")
{
    // source_id doesn't look like a record ID — use work_id if available
    work_id.as_deref().unwrap_or(&source_id).to_string()
} else {
    source_id.clone()
};
```

##### Fix M10-M12 — Remove dead ActionResult variants

**Problem:** `ActionResult::DuplicateDetected`, `PhaseCompleted`, `GoalCompleted` are declared and matched in `format_action_summary` but never returned by any code path in `execute_action`.

**Fix:** All three variants are confirmed dead — declared in the enum and matched in `format_action_summary` (coordinator.rs:1251, implementer.rs:414) but never returned by any `execute_action` code path. `DuplicateDetected` has match arms in two `format_action_summary` functions but zero `return Ok(ActionResult::DuplicateDetected {...})` anywhere in the codebase.

Remove all three variants and their match arms in both `format_action_summary` functions (coordinator.rs and implementer.rs).

```rust
// agents/executor.rs — remove unused variants from ActionResult enum
// DELETE: DuplicateDetected { existing_id: String, title: String },
// DELETE: PhaseCompleted { phase_id: String, next_phase_id: Option<String> },
// DELETE: GoalCompleted { goal_id: String, phases_completed: usize },

// agents/coordinator.rs — remove match arms in format_action_summary
// agents/implementer.rs — remove match arms in format_action_summary
```

Note: If duplicate detection is implemented in a future MVP, `DuplicateDetected` can be re-added. Dead variants with match arms create a false sense of coverage.

### Typed IPC Param Structs (Phase 5, Incremental)

To prevent future mismatches, introduce typed param structs for the highest-traffic IPC methods. These structs are shared between executor and handler, providing compile-time agreement.

```rust
// ipc/params.rs (new file)

/// Params for bundle.create
#[derive(Debug, Serialize, Deserialize)]
pub struct BundleCreateParams {
    pub work_id: String,
    pub branch_name: String,
    pub claims: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub base_tick_id: Option<String>,
    #[serde(default)]
    pub touched_paths: Vec<String>,
}

/// Params for learning.create
#[derive(Debug, Serialize, Deserialize)]
pub struct LearningCreateParams {
    pub content: String,
    pub scope: LearningScope,
    pub source_id: String,
    #[serde(default)]
    pub applicable_roles: Option<Vec<Role>>,
    #[serde(default)]
    pub resource_tags: Option<Vec<String>>,
}

/// Params for worktree.refresh
#[derive(Debug, Serialize, Deserialize)]
pub struct WorktreeRefreshParams {
    pub work_id: String,
    #[serde(default = "default_base_ref")]
    pub new_base_ref: String,
}

fn default_base_ref() -> String { "HEAD".to_string() }
```

The executor uses `serde_json::to_value(BundleCreateParams { ... })` instead of `json!({})`. The handler uses `serde_json::from_value::<BundleCreateParams>(req.params)` instead of manual `get().and_then(as_str())`. Type errors become compile errors.

This is introduced for `bundle.create`, `learning.create`, and `worktree.refresh` in MVP7. Remaining methods are converted in future work.

### Data Model

**`Bundle`** — `claims` field changes from `String` to `Vec<String>`. Breaking change for existing TaskStore data — migration: on load, if `claims` is a String, wrap in `vec![claims]`. The field needs `#[serde(deserialize_with)]` to use the custom deserializer.

```rust
// domain/bundle.rs — Bundle struct
pub struct Bundle {
    // ...
    #[serde(deserialize_with = "deserialize_claims")]
    pub claims: Vec<String>,
    // ...
}

// Backward-compatible deserialization (free function, not impl method)
fn deserialize_claims<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    struct ClaimsVisitor;
    impl<'de> de::Visitor<'de> for ClaimsVisitor {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a string or array of strings")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(if v.is_empty() { Vec::new() } else { vec![v.to_string()] })
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(s) = seq.next_element::<String>()? { v.push(s); }
            Ok(v)
        }
    }
    deserializer.deserialize_any(ClaimsVisitor)
}
```

**`Learning`** — no structural changes. `applicable_roles` and `resource_tags` fields already exist; the handler just wasn't populating them.

**`LearningScope`** — adds `#[serde(alias)]` attributes. No structural change.

**`HierarchyStatus`** — adds `#[serde(alias)]` attributes. No structural change.

**New file: `ipc/params.rs`** — typed param structs for compile-time IPC safety.

### API Changes

No new IPC methods. Behavioral changes:

| Method | Change |
|--------|--------|
| `bundle.create` | `claims` parsed as array (backward-compat: also accepts string) |
| `learning.create` | `applicable_roles` and `resource_tags` now read and persisted |
| `worktree.refresh` | (No API change — callers must send `work_id`, which was always required) |

### Implementation Plan

#### Phase 1: Work Lifecycle & Coordinator Visibility

**Files:** `src/agents/integrator.rs`, `src/agents/coordinator.rs`

1. In `run_integrator_cycle()`, after the bundle `Integrating → Merged` loop (line ~521), add WI `InReview → Integrated` transitions for each merged bundle's parent WI
2. In `build_state_summary()`, add "Recently Merged Bundles" section after the existing Bundles section
3. Test: After Integrator publishes a Tick, parent WIs are in Integrated status
4. Test: `build_state_summary()` includes recently-merged bundles with WI linkage
5. Test: Coordinator LLM sees Integrated WIs and can transition to Done
6. Test: WI already in a status other than InReview is skipped (not double-transitioned)

#### Phase 2: IPC Data-Loss Fixes

**Files:** `src/domain/bundle.rs`, `src/daemon/handlers.rs`, `src/domain/learning.rs`, `src/agents/integrator.rs`, `src/agents/coordinator.rs`

1. Change `Bundle.claims` from `String` to `Vec<String>` with backward-compat deserializer
2. Update `Bundle::new()` signature and all callers
3. Update `handle_bundle_create` to parse claims as JSON array
4. Update `handle_bundle_update` to parse claims consistently
5. Fix coordinator.rs retry-exhaustion learning scope: `"Phase"` → `"phase"`
6. Add `#[serde(alias)]` to `LearningScope` variants
7. In `handle_learning_create`, parse and set `applicable_roles` and `resource_tags`
8. Fix Integrator `AutoReplayAndVerify` to pass `work_id` and `new_base_ref` to `worktree.refresh`
9. Test: ProposeBundle with `Vec<String>` claims → Bundle stores all claims
10. Test: ProposeBundle with empty claims → Bundle stores empty Vec
11. Test: Learning created with scope "Phase" (PascalCase) succeeds via alias
12. Test: Learning created with applicable_roles → roles stored in Learning
13. Test: Learning created with resource_tags → tags stored in Learning
14. Test: AutoReplayAndVerify sends work_id → worktree.refresh succeeds

#### Phase 3: Integrator Merge-Failure & Retry Enforcement

**Files:** `src/domain/tick.rs`, `src/agents/integrator.rs`, `src/agents/coordinator.rs`

1. Add `Sealing → Failed` transition rule to `tick_transitions()` in tick.rs — prerequisite for B3
2. In merge-failure path, transition Tick via IPC (not direct mutation) — fixes B3
3. In merge-failure path, transition all Bundles `Integrating → Rejected` — fixes B2
4. Remove the direct `tick.status = TickStatus::Failed` mutation from merge-failure path
5. Refactor retry enforcement: only count successful `AgentSpawned` results as attempts — fixes B4
6. Decrement attempts on `DependencyNotMet` AND on `ActionError` / other non-spawn results
7. Test: `Sealing → Failed` is a valid tick FSM transition
8. Test: Merge failure → Tick is Failed (via IPC, with event + persistence)
9. Test: Merge failure → Bundles are Rejected (not orphaned in Integrating)
10. Test: Failed AssignAgent (WI already InProgress) does not burn a retry slot
11. Test: DependencyNotMet does not burn a retry slot
12. Test: Successful spawn increments attempt count

#### Phase 4: Persistence & Scoping

**Files:** `src/agents/coordinator.rs`

1. Add clone-then-drop-then-persist to `mark_phase_record_complete()`
2. Scope `find_pending_draft_for_validation()` to current active hierarchy chain
3. Test: After PhaseGate, Phase record is persisted as Complete in TaskStore
4. Test: Old Draft from a different Plan is NOT returned by `find_pending_draft_for_validation()`
5. Test: Draft child of active Plan IS returned

#### Phase 5: Typed IPC Params, Naming Cleanup, Dead Code

**Files:** `src/ipc/params.rs` (new), `src/domain/plan.rs`, `src/daemon/handlers.rs`, `src/agents/executor.rs`, `src/agents/mod.rs`

1. Create `ipc/params.rs` with `BundleCreateParams`, `LearningCreateParams`, `WorktreeRefreshParams`
2. Add `#[serde(alias)]` to `HierarchyStatus` variants
3. Normalize `files_changed` / `touched_paths` in both create and update handlers
4. Fix Reviewer source_id to use WI ID instead of title (executor defense-in-depth + prompt fix)
5. Remove `DuplicateDetected`, `PhaseCompleted`, and `GoalCompleted` from `ActionResult` enum
6. Remove their match arms in `format_action_summary` (both coordinator.rs and implementer.rs)
7. Migrate `bundle.create` handler to use `BundleCreateParams` struct
8. Migrate `learning.create` handler to use `LearningCreateParams` struct
9. Migrate `worktree.refresh` handler to use `WorktreeRefreshParams` struct
10. Test: `BundleCreateParams` serialization matches handler deserialization roundtrip
11. Test: `LearningCreateParams` with all optional fields populated roundtrips correctly
12. Test: `HierarchyStatus` deserializes from both "draft" and "Draft"
13. Test: Both `"touched_paths"` and `"files_changed"` are accepted in bundle.create

## Alternatives Considered

### Alternative 1: Fix Only the 2 Critical Blockers

- **Description:** Ship C1 and C2 only, defer everything else.
- **Pros:** Smallest change set. WI lifecycle unblocked.
- **Cons:** Every bundle still has empty claims (M1). Every learning loses its roles and tags (M3, M4). Retry exhaustion learnings are silently dropped (M2). The merge-failure path still orphans bundles (B2). The same class of bugs will recur because the root cause (untyped IPC params) is unaddressed.
- **Why not chosen:** The data-loss bugs are actively degrading system intelligence (learnings are the feedback loop), and the merge-failure bugs will brick the pipeline on the first conflict. Fixing only C1/C2 produces a system that advances WIs but has no working learning system and fragile merge handling.

### Alternative 2: Full IPC Rewrite with Typed Params Everywhere

- **Description:** Convert all 30+ IPC methods to typed param structs before fixing any bugs.
- **Pros:** Prevents all future mismatches at compile time. Clean architecture.
- **Cons:** Massive change set touching every handler and every executor call site. High risk of introducing regressions. Blocks bug fixes on an infrastructure change.
- **Why not chosen:** Incremental is better. Fix the bugs first (Phases 1-4), then introduce typed params for the 3 highest-risk methods (Phase 5). Future MVPs convert remaining methods.

### Alternative 3: Add Coordinator Permission for InReview → Integrated (instead of Integrator doing it)

- **Description:** Add `Role::Coordinator` to the `InReview → Integrated` transition rule, letting the Coordinator LLM do it.
- **Pros:** Simpler code change — just add one `TransitionRule`.
- **Cons:** Requires the Coordinator LLM to (a) notice that bundles are merged, (b) figure out which WI corresponds to which Bundle, and (c) issue the transition. This adds LLM iterations and depends on LLM reliability for a critical lifecycle step. The Integrator already knows exactly which bundles were merged and which WIs they belong to — it's deterministic.
- **Why not chosen:** Deterministic > LLM-dependent for lifecycle-critical transitions. The Integrator already has the context; making it do the transition is both more reliable and more efficient.

## Technical Considerations

### Dependencies

- **Internal:** TaskStore Record trait (existing), FSM infrastructure (existing), IPC framework (existing)
- **External:** None new. All changes use existing dependencies.

### Performance

- **C1 (WI transitions):** O(b) bridge requests where b is bundles in the Tick (typically 1-5). Runs once per Tick publish.
- **C2 (merged bundles in summary):** O(b) scan of bundles HashMap. Negligible.
- **M1 (claims parsing):** Switching from `as_str()` to array parsing. Negligible.
- **Phase 5 (typed params):** `serde_json::from_value` instead of manual parsing. Same or better performance.

### Security

No security implications. All changes are internal to the daemon process. No new external interfaces.

### Testing Strategy

Each phase includes unit tests. Key regression scenarios:

1. **WI lifecycle end-to-end:** Bundle merge → WI Integrated → Coordinator transitions to Done
2. **Claims roundtrip:** Vec<String> claims survive executor → handler → Bundle → TaskStore → read-back
3. **Learning scope aliases:** "Phase", "phase", "PHASE" all deserialize to LearningScope::Phase
4. **Learning field preservation:** applicable_roles and resource_tags survive creation
5. **Merge failure cleanup:** Bundles rejected, Tick failed via FSM, repo clean
6. **Retry slot conservation:** Failed assignments don't burn slots
7. **Phase persistence:** Complete status survives simulated crash/reload
8. **Draft scoping:** Only hierarchy-chain Drafts are found
9. **Typed param roundtrip:** Executor serialization matches handler deserialization

### Rollout Plan

1. **Phase 1** (lifecycle blockers) — deploy first; unblocks WI completion
2. **Phase 2** (data loss) — deploy second; restores learning system functionality
3. **Phase 3** (merge/retry) — deploy third; improves reliability under failure
4. **Phase 4** (persistence/scoping) — deploy fourth; crash recovery and correctness
5. **Phase 5** (typed params) — deploy last; preventive infrastructure

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `Bundle.claims` type change breaks existing TaskStore data | Medium | High | Backward-compat deserializer accepts both String and Vec<String>. Existing empty strings become empty Vec. |
| Integrator WI transitions fail (WI not in InReview) | Low | Low | Check status before transitioning. Skip WIs not in InReview. Log warning. |
| Multiple bundles per WI — Integrator tries to transition same WI twice | Low | Low | HashSet dedup on WI IDs + status check. Second attempt sees Integrated, skips. |
| Coordinator crash between retry increment and decrement | Low | Low | Worst case: one fewer retry on recovery. Acceptable — max_retries=3 means 2 retries instead of 3. |
| serde(alias) causes unexpected deserialization | Low | Low | Aliases only add accepted inputs; serialization output unchanged (always lowercase). |
| Removing ActionResult variants breaks downstream match arms | Low | Medium | Compiler will catch any remaining match arms. Only remove variants confirmed unused. |
| Typed param structs diverge from handler expectations | Low | Low | Both sides use the same struct — compile-time agreement. Tests verify roundtrip. |
| Retry slot decrement on ActionError over-corrects | Medium | Medium | Only decrement for specific non-spawn results. Log when decrement happens for observability. |
| M9 source_id heuristic breaks if ID prefix convention changes | Low | Low | Heuristic is defense-in-depth only. Primary fix is the Reviewer prompt. Document prefix convention. |
| `Sealing → Failed` rule enables new failure paths not previously possible | Low | Medium | The path was already taken via direct mutation. The new rule just makes it FSM-legal. No new failure modes. |

## Open Questions

- [ ] **Should Bundle.claims be `Vec<String>` or `String` with join?** Recommendation: `Vec<String>`. The AgentAction already uses Vec, the LLM naturally produces arrays, and individual claims are useful for filtering/matching. Joining into a String loses structure.
- [ ] **Should the Integrator also transition WIs Integrated → Done?** Recommendation: No. Keep it at `InReview → Integrated` only. The Coordinator should retain control over the `Integrated → Done` transition since it's the one managing phase completion and may want to inspect the integration result before marking Done.
- [ ] **Should all 30+ IPC methods get typed params in MVP7?** Recommendation: No. Only the 3 methods with confirmed bugs. Future MVPs convert remaining methods incrementally.

## References

- [MVP4 Design Doc](2026-02-26-loopr-v3-mvp4.md) — multi-level RWL, Coordinator, Integrator
- [MVP5 Design Doc](2026-02-28-loopr-v3-mvp5-coordinator-sequencing.md) — Coordinator control loop
- [MVP6 Design Doc](2026-02-28-loopr-v3-mvp6-structural-fixes.md) — 12 structural fixes
- [E2E Blockers](2026-02-27-loopr-v3-e2e-blockers.md) — pipeline integration fixes
- [Audit Fixes](2026-02-27-loopr-v3-audit-fixes.md) — 23 defects found and fixed
