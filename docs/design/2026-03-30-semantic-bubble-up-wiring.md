# Design Document: Semantic Bubble-Up Wiring

**Author:** Scott A. Idler
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 3/3

## Summary

Connect three existing but unwired subsystems in the Coordinator's decision tree: (1) emit `ReviseParent` instead of `NeedHelp` when decomposition attempts are exhausted, (2) inject coverage gap diagnostics into regeneration prompts so revised parents address the actual gaps, (3) track bubble-up depth to prevent infinite revision loops. 90% of the infrastructure exists - this is integration wiring only.

## Problem Statement

### Background

Loopr's orchestration pipeline decomposes goals hierarchically: Plan -> Specs -> Phases -> Works. At each boundary, a Coverage Evaluator (LLM-based) checks whether the children semantically cover the parent's requirements. When coverage is Incomplete, the Coordinator re-decomposes. When re-decomposition attempts are exhausted, the system should revise the *parent* (bubble up) - because the problem is often ambiguity in the parent, not in the children.

All the pieces for this pipeline exist and are individually functional:
- `EvaluateCoverage` action + executor implementation (`agents/executor.rs:1180`)
- `ReviseParent` action + executor implementation (`agents/executor.rs:1353`)
- `decomposition_attempts` tracking in `CoordinatorState` with increment/reset/get methods
- `is_decomposition_cap_reached()` detection in `generation.rs:882`
- `find_incomplete_decomposition()` with gap extraction in `generation.rs:828`
- `max_decomposition_attempts` and `max_bubble_up_depth` config values

### Problem

The pieces are not connected. Specifically:

1. **Case 0 in `build_generation_footer()` (coordinator.rs:316-329)** emits `NeedHelp` when `is_decomposition_cap_reached()` returns `Some`. It should emit `ReviseParent` to push the parent back to Draft, then re-attempt decomposition. `NeedHelp` should only fire when bubble-up depth is exhausted or we've reached the Plan level (can't revise above Plan).

2. **Prompt builders don't carry diagnostic context.** When `ReviseParent` transitions a parent to Draft and the Coordinator regenerates it, the regeneration prompt has no knowledge of *why* the revision happened. The gap descriptions from the coverage report must flow into the next `build_spec_prompt()` / `build_phase_prompt()` call so the LLM can fix the actual gaps.

3. **No bubble-up depth tracking.** `max_bubble_up_depth: 2` exists in config but `CoordinatorState` has no field tracking how many times we've bubbled up for a given goal. Without this, a Plan with persistently vague acceptance criteria would bubble up infinitely.

### Goals

- Replace `NeedHelp` with `ReviseParent` in Case 0 when bubble-up is possible
- Inject coverage gap diagnostics from `ReviseParent` Learnings into regeneration prompts
- Add `bubble_up_count` to `CoordinatorState` with `max_bubble_up_depth` guard
- `NeedHelp` only fires when bubble-up depth exhausted or at Plan level
- Zero changes to `EvaluateCoverage`, `ReviseParent`, or `ToolRunner` - those work correctly

### Non-Goals

- Changing the Coverage Evaluator's LLM logic (it works)
- Collaborative Plan interview wiring (`Interviewing` state) - separate concern
- Changing the Work-level pipeline (Implementer/Reviewer/Integrator)
- Adding new IPC handlers or bridge endpoints

## Proposed Solution

### Overview

Three targeted changes to the Coordinator's decision tree, each building on existing infrastructure:

1. **Wire 1 (Case 0 -> ReviseParent):** When `is_decomposition_cap_reached()` fires, check bubble-up depth. If under limit and parent is not a Plan, emit `ReviseParent` with gap diagnostic. If at limit or at Plan level, emit `NeedHelp`.

2. **Wire 2 (Diagnostic injection):** When a parent is revised (transitioned back to Draft), the `ReviseParent` executor already creates a Learning with the diagnostic. The Coordinator's `build_generation_footer()` Case 1 already passes `learnings` to prompt builders. The missing link: ensure the Learning created by `ReviseParent` is picked up by the next generation cycle. This may already work via the existing Learning query - needs verification.

3. **Wire 3 (Depth tracking):** Add `bubble_up_count: u32` to `CoordinatorState`. Increment on each `ReviseParent` emission. Reset when the goal succeeds. Guard with `max_bubble_up_depth` from config.

### Architecture

```
build_generation_footer() Decision Tree (updated Case 0):

  is_decomposition_cap_reached()?
    ├── Some((collection, parent_id))
    │     ├── collection == "plan"?
    │     │     └── YES: NeedHelp (can't revise above Plan)
    │     ├── bubble_up_count >= max_bubble_up_depth?
    │     │     └── YES: NeedHelp (depth exhausted)
    │     └── ELSE: ReviseParent(collection, parent_id, gaps)
    │           ├── increment bubble_up_count
    │           └── reset decomposition_attempts for parent_id
    └── None: continue to Case 0b
```

### Data Model

#### CoordinatorState Addition

```rust
// src/domain/coordinator_state.rs

pub struct CoordinatorState {
    // ... existing fields ...

    /// Number of times we've bubbled up (revised a parent due to child decomposition failure).
    /// Guarded by config.strategy.max_bubble_up_depth.
    pub bubble_up_count: u32,
}

impl CoordinatorState {
    // ... existing methods ...

    pub fn increment_bubble_up(&mut self) -> u32 {
        self.bubble_up_count += 1;
        self.updated_at = now_millis();
        self.bubble_up_count
    }

    pub fn reset_bubble_up(&mut self) {
        self.bubble_up_count = 0;
        self.updated_at = now_millis();
    }
}
```

### API Design

#### Updated Case 0 in build_generation_footer

**Critical principle:** `build_generation_footer()` is a pure prompt-building function. It MUST NOT mutate `CoordinatorState`. State mutation (incrementing `bubble_up_count`, resetting `decomposition_attempts`) happens in `executor.rs` inside the `ReviseParent` handler, only after the parent is successfully transitioned to Draft. This prevents state corruption if the LLM hallucinates or fails to parse the action.

```rust
// src/agents/coordinator.rs - build_generation_footer() Case 0
// NOTE: This function is PURE - no state mutation. Just builds the prompt.

if let (Some(cs), Some(max_da)) = (coord_state, max_decomposition_attempts)
    && let Some((collection, parent_id)) = generation::is_decomposition_cap_reached(stores, cs, max_da)
{
    let max_bud = stores.config.strategy.max_bubble_up_depth;

    // Can't revise above Plan, and can't exceed bubble-up depth
    if collection == "plan" || cs.bubble_up_count >= max_bud {
        agent_log.info(&format!(
            "bubble-up exhausted for {} {} (count={}, max={}), signaling need_help",
            collection, parent_id, cs.bubble_up_count, max_bud
        ));
        return Some(format!(
            "Coverage evaluation for {collection} ({parent_id}) has failed {max_da} times. \
             Bubble-up depth exhausted ({}/{max_bud}). Human review needed.\n\
             [{{\"action\": \"need_help\", \"reason\": \"Decomposition failed for {collection} {parent_id} after {max_da} attempts and {} bubble-ups\"}}]",
            cs.bubble_up_count
        ));
    }

    // Bubble up: revise the parent
    let gaps = generation::get_coverage_gaps(stores, &collection, &parent_id);
    let diagnostic = if gaps.is_empty() {
        format!("Decomposition of {} {} failed {} times", collection, parent_id, max_da)
    } else {
        format!(
            "Decomposition of {} {} failed {} times. Coverage gaps:\n{}",
            collection, parent_id, max_da, gaps.join("\n")
        )
    };

    agent_log.info(&format!(
        "bubbling up: ReviseParent {} {} (bubble_up_count={}/{})",
        collection, parent_id, cs.bubble_up_count + 1, max_bud
    ));

    return Some(format!(
        "## Bubble-Up Required\n\n\
         Coverage evaluation for {collection} ({parent_id}) has failed {max_da} times.\n\
         The children cannot fix this - the parent needs revision.\n\n\
         ### Diagnostic:\n{diagnostic}\n\n\
         Respond with:\n\
         [{{\"action\": \"revise_parent\", \"collection\": \"{collection}\", \"id\": \"{parent_id}\", \
         \"reason\": \"decomposition failed {max_da} times\", \
         \"diagnostic\": \"{diagnostic_escaped}\"}}]"
    ));
}
```

#### State Mutation in ReviseParent Executor

All state changes happen here, only after successful transition:

```rust
// src/agents/executor.rs - inside AgentAction::ReviseParent handler

// After successful transition to Draft:
coord_state.increment_bubble_up();
coord_state.reset_decomposition_attempts(&id);

// Create Learning with scope derived from collection (not hardcoded "plan")
let scope = match collection.as_str() {
    "plans" | "plan" => "plan",
    "specs" | "spec" => "spec",
    "phases" | "phase" => "phase",
    _ => "plan",  // fallback
};
let learning_content = format!(
    "Bubble-up revision for {}/{}: {}\nDiagnostic: {}",
    collection, id, reason, diagnostic
);
bridge.request("learning.create", serde_json::json!({
    "content": learning_content,
    "scope": scope,
    "source_id": id,
}));
```

#### Gap Extraction Helper

```rust
// src/agents/generation.rs

/// Extract coverage gap descriptions for a parent from the latest coverage report.
pub fn get_coverage_gaps(stores: &Stores, collection: &str, parent_id: &str) -> Vec<String> {
    // Query the latest CoverageReport for this parent
    // Extract gap descriptions
    // Return formatted strings
    // (This may already exist in find_incomplete_decomposition - factor out if so)
}
```

#### Diagnostic Flow into Regeneration

The `ReviseParent` executor (executor.rs:1353) already creates a Learning:
```rust
let learning_content = format!(
    "Bubble-up revision for {}/{}: {}\nDiagnostic: {}",
    collection, id, reason, diagnostic
);
bridge.request("learning.create", ...);
```

The prompt builders (`build_spec_prompt`, etc.) already accept `learnings: &[String]` and include them. The `build_generation_footer()` Case 1 queries learnings before passing to builders:

```rust
// coordinator.rs - existing code in Case 1 around lines 360-380
let learnings = query_learnings(stores);  // Already exists
let prompt = build_spec_prompt(plan, &learnings, ...);
```

**Verified: Learnings are NOT currently passed to prompt builders.** Case 1 in `build_generation_footer()` (coordinator.rs:360-375) passes empty `&[]` for the `learnings` parameter in every prompt builder call. This means even though `ReviseParent` creates a Learning, it won't appear in the regeneration prompt.

**The fix:** Query Learnings from `stores.read_learnings()` in Case 1 and pass them to the prompt builders. Filter by the scope matching the document being generated: when generating a Spec, query `scope == "spec"` learnings; when generating a Phase, query `scope == "phase"` learnings; etc. The `ReviseParent` executor derives the Learning scope from the revised collection (not hardcoded to "plan"), so a Spec revision creates a "spec"-scoped Learning that the Spec prompt builder will pick up.

### Implementation Plan

#### Phase 1: Bubble-Up Depth Tracking

**Files modified:**
- `src/domain/coordinator_state.rs` - add `bubble_up_count: u32` field, `increment_bubble_up()`, `reset_bubble_up()` methods. Default to 0 in constructor.
- `src/daemon/handlers.rs` - in the `coordinator.set_goal` handler, call `coord_state.reset_bubble_up()` so every new goal starts with a clean slate. Without this, a persisted `bubble_up_count` from a previous run would carry over and prematurely trigger NeedHelp.

**Outcome:** CoordinatorState can track bubble-up depth, reset per-goal.

#### Phase 2: Replace NeedHelp with ReviseParent

**Files modified:**
- `src/agents/coordinator.rs` - Case 0 in `build_generation_footer()`: emit `ReviseParent` prompt when under depth limit, `NeedHelp` when exhausted. This function stays pure (no state mutation).
- `src/agents/executor.rs` - `ReviseParent` handler: after successful transition, call `coord_state.increment_bubble_up()` and `coord_state.reset_decomposition_attempts(parent_id)`. Derive Learning scope from collection (not hardcoded "plan").
- `src/agents/generation.rs` - extract `get_coverage_gaps()` helper if not already factored out

**Outcome:** Decomposition cap fires `ReviseParent` which transitions parent to Draft, creates scope-matched diagnostic Learning, resets decomposition attempts for fresh retries. All state mutation happens only on successful execution.

#### Phase 3: Wire Learnings into Prompt Builders + Tests

**Files modified:**
- `src/agents/coordinator.rs` - Case 1 in `build_generation_footer()`: query Learnings from `stores.read_learnings()` filtered by Plan scope, pass to prompt builders instead of empty `&[]`
- Add integration test: decomposition failure -> bubble-up -> parent revision -> re-decomposition with diagnostic context

**Outcome:** Full bubble-up loop works end-to-end. Diagnostic from `ReviseParent` appears in the regeneration prompt.

## Alternatives Considered

### Alternative 1: Always NeedHelp, Let Human Revise

- **Description:** Keep the current behavior. When decomposition fails, always escalate to the user.
- **Pros:** Safest. Human always in the loop. No risk of infinite revision.
- **Cons:** Defeats autonomous operation. Most decomposition failures are fixable by the system (vague parent -> more specific parent -> better children). Requires human for every coverage gap.
- **Why not chosen:** The entire point of the bubble-up architecture is autonomous self-correction. NeedHelp should be the last resort, not the first.

### Alternative 2: Skip Parent Revision, Just Retry Children

- **Description:** When decomposition fails max times, retry with stronger prompting but don't touch the parent.
- **Pros:** Simpler. No state tracking needed.
- **Cons:** If the parent is vague, no amount of retrying children will fix it. Jim West's principle: bad output from a focused agent means the input was bad.
- **Why not chosen:** This is what the system currently does (Case 0b re-decomposition). It works for minor gaps but fails for fundamentally ambiguous parents.

## Technical Considerations

### Dependencies

No new crate dependencies. All infrastructure exists.

### Performance

- Bubble-up adds one additional LLM call (to regenerate the revised parent) per bubble-up event. At most `max_bubble_up_depth` (default 2) additional calls per goal.
- `ReviseParent` already handles the transition + Learning creation efficiently.

### Testing Strategy

- **Unit tests:** `bubble_up_count` increment/reset in CoordinatorState
- **Decision tree test:** `build_generation_footer` Case 0 returns `ReviseParent` prompt when under depth limit, `NeedHelp` when exhausted
- **Integration test:** Full cycle: create Plan -> decompose Specs -> coverage Incomplete -> re-decompose -> still Incomplete -> ReviseParent -> Plan back to Draft -> re-decompose with diagnostic -> coverage Complete
- **Existing tests:** All current coordinator, generation, and executor tests must continue passing

### Rollout Plan

All three phases can ship as a single commit since they're tightly coupled. The bubble-up path is only triggered when coverage evaluation fails repeatedly, which is a rare event - low risk of regression.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| ReviseParent diagnostic Learning not picked up by prompt builders | Confirmed | High | Phase 3 wires `stores.read_learnings()` into Case 1 prompt builders |
| Infinite bubble-up loop | Low | High | `max_bubble_up_depth` guard (default 2) + NeedHelp fallback |
| Revised parent generates identical bad children | Medium | Medium | Diagnostic context in prompt gives LLM specific gaps to fix |
| Plan-level bubble-up (nowhere to go) | Low | Low | Explicit guard: `collection == "plan"` -> NeedHelp immediately |

## Open Questions

- [x] ~~Does `query_learnings()` already pick up Learnings created by `ReviseParent`?~~ Verified: No. Case 1 passes empty `&[]` for learnings. Fix: query `stores.read_learnings()` and pass results to prompt builders.
- [ ] Should `bubble_up_count` be per-parent or per-goal? Per-goal is simpler and sufficient (if we bubble up twice for the whole goal, something is fundamentally wrong).
- [x] ~~Should `decomposition_attempts` be reset for the parent after `ReviseParent` fires?~~ Yes. The `ReviseParent` executor should call `coord_state.reset_decomposition_attempts(parent_id)` after transitioning to Draft. The revised parent is effectively a new document and deserves fresh decomposition attempts.

## References

- `docs/design/2026-03-03-semantic-decomposition.md` - original Coverage Evaluator + bubble-up design (MVP9)
- `docs/design/2026-03-21-coverage-bubble-up-and-headless-mode.md` - partial implementation status
- `docs/design/remaining-gaps.md` - gap inventory
- `docs/next-steps.md` item #3 - roadmap entry
- `src/agents/coordinator.rs:307-489` - `build_generation_footer()` decision tree
- `src/agents/executor.rs:1180-1232` - `EvaluateCoverage` executor
- `src/agents/executor.rs:1353-1405` - `ReviseParent` executor
- `src/agents/generation.rs:828-909` - decomposition detection helpers
