# Design Document: Test-Run Observation Fixes

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Three pre-existing issues surfaced during the `test-run.sh` integration test: (1) the LLM consistently wraps empty `[]` responses in markdown fences, burning a requery on every idle iteration, (2) when the Integrator rejects a stale bundle, the parent Work stays stuck in `InReview` indefinitely because no actor resets it, and (3) the coordinator's state summary exceeds its 3000-token budget and gets tail-truncated, dropping the newest (most relevant) status information. This doc proposes targeted fixes for all three.

## Problem Statement

### Background

Loopr's coordinator runs an FSM loop where each iteration calls the LLM, parses the JSON response via `parse_actions()`, executes actions, and sleeps. The Integrator merges accepted bundles to main and transitions Works to `Integrated`. The coordinator's context builder assembles a prompt from hierarchy, state summary, learnings, and footer sections, each with a token budget.

### Problem 1: Markdown Fence Parse Failures

The LLM (Claude) consistently wraps empty JSON arrays in markdown fences:

```
```json
[]
```​
```

The `parse_actions()` function (implementer.rs:68-110) tries direct parse first, then falls back to bracket-finding. The direct parse fails on the fenced response because the fence text is not valid JSON. The bracket-finding fallback finds `[]` inside the fences but skips it because `!actions.is_empty()` rejects empty arrays (this guard exists to avoid returning empty results from partial bracket matches). The function returns `Err`, triggering a requery. The requery always succeeds because the LLM responds with bare `[]`.

Note: Non-empty fenced arrays (e.g., ```` ```json\n[{"action":"done",...}]\n``` ````) parse correctly via the bracket-finding fallback because `!actions.is_empty()` passes. Only the empty-array-in-fences case is broken.

**Impact:** Every idle coordinator iteration wastes one LLM call (~2s latency, API cost) on a requery that always succeeds.

### Problem 2: Stale Bundle Leaves Work Stuck in InReview

When main advances (a new Tick is published), previously-accepted bundles become stale because their `base_tick_id` no longer matches. The Integrator's `ReplanAtSafePoint` policy rejects the bundle (`Accepted → Rejected`) and emits a `bundle.stale_replan_needed` event, but:

1. No listener handles the `bundle.stale_replan_needed` event
2. The Work stays in `InReview` — no actor transitions it back to `Ready`
3. The coordinator sees `InReview` with no bundle to triage and emits `[]`
4. The Work is permanently stuck until phase timeout or manual intervention

**Impact:** Any phase with >1 Work that reaches the integrator sequentially will have N-1 Works stuck in `InReview` after the first merge advances main.

### Problem 3: State Summary Token Budget Overflow

The coordinator's state summary (`build_state_summary_with_sla`) builds sections for Plans, Specs, Phases, Works, Bundles, Ticks, Agents, and Locks. With SLA annotations, a phase with 3+ Works easily exceeds the 3000-token budget. The `truncate_prose()` function drops the **tail** (newest content), meaning the most relevant sections — Works, Bundles, active Agents — are truncated first, while stale Plan/Spec headers survive.

**Impact:** The coordinator loses visibility into current Work statuses and active Agents, making it unable to take meaningful actions. It falls back to `[]` every iteration.

### Goals

- Eliminate requery waste on markdown-fenced empty responses
- Automatically recover Works stuck in `InReview` after bundle rejection
- Ensure the coordinator always sees current Work/Bundle/Agent status within budget

### Non-Goals

- Changing the LLM's behavior to not emit markdown fences (unreliable)
- Implementing the full `AutoReplayAndVerify` stale policy (future work)
- Restructuring the context builder's token budget system (working fine for other sections)

## Proposed Solution

### Fix 1: Strip Markdown Fences Before Parsing

Add a `strip_markdown_fences()` preprocessing step at the top of `parse_actions()`, before any parse attempts. This normalizes the response text by removing ```` ```json ```` / ```` ``` ```` wrappers.

```rust
// In implementer.rs, at the top of parse_actions():

/// Strip markdown code fences from LLM responses.
/// Handles ```json, ```, and variants with language tags.
fn strip_markdown_fences(response: &str) -> String {
    let trimmed = response.trim();
    // Match opening fence: ``` optionally followed by a language tag
    if let Some(rest) = trimmed.strip_prefix("```") {
        // Skip the language tag line (e.g., "json\n")
        let after_tag = rest
            .find('\n')
            .map(|i| &rest[i + 1..])
            .unwrap_or(rest);
        // Strip closing fence
        let content = after_tag
            .trim_end()
            .strip_suffix("```")
            .unwrap_or(after_tag);
        content.trim().to_string()
    } else {
        trimmed.to_string()
    }
}
```

Apply it as the first step in `parse_actions()`:

```rust
pub fn parse_actions(response: &str, agent_log: &AgentLogger) -> Result<Vec<AgentAction>> {
    agent_log.debug(&format!("parse_actions(response_len={})", response.len()));
    let stripped = strip_markdown_fences(response);
    let normalized = normalize_action_keys(&stripped);
    let response = &normalized;
    // ... rest unchanged
}
```

This also fixes the secondary issue: the existing bracket-finding fallback rejects empty `[]` because of the `!actions.is_empty()` guard. After stripping fences, the direct parse will succeed for `[]`, returning an empty `Vec<AgentAction>` — which is the correct result for "no actions needed."

### Fix 2: Integrator Transitions Work Back to Ready on Bundle Rejection

When the Integrator rejects a bundle (stale, merge conflict, or validation failure), it should also transition the parent Work from `InReview` back to `Ready`. This allows the worker pool to pick it up for a fresh attempt with the updated main branch.

Add a helper called after every bundle rejection:

```rust
/// After rejecting a bundle, reset the parent Work to Ready so it can be re-assigned.
fn reset_work_after_bundle_rejection(
    &self,
    work_id: &str,
    reason: &str,
) {
    // Transition Work: InReview → Ready (override transition, coordinator role)
    let resp = self.ctx.bridge.request(
        "work.transition",
        serde_json::json!({
            "id": work_id,
            "target_status": "Ready",
            "role": "coordinator",
            "override": true,
        }),
    );
    if resp.is_error() {
        self.ctx.warn(&format!(
            "failed to reset Work {} to Ready after bundle rejection: {:?}",
            work_id, resp.error
        ));
    } else {
        self.ctx.info(&format!(
            "Work {} reset to Ready after bundle rejection ({})", work_id, reason
        ));
    }

    // Create a Learning so the next implementer knows why
    let _ = self.ctx.bridge.request(
        "learning.create",
        serde_json::json!({
            "content": format!("Bundle rejected ({}). Work reset to Ready for retry with updated main branch.", reason),
            "scope": "phase",
            "source_id": work_id,
        }),
    );
}
```

Call sites — after each bundle rejection in the Integrator:

1. **Stale rejection** (line ~314, ~341): `self.reset_work_after_bundle_rejection(&wi_id, "stale base tick")`
2. **Merge conflict** (line ~534-549 loop): `self.reset_work_after_bundle_rejection(&wi_id, "merge conflict")`
3. **Validation failure** (line ~714-729 loop): `self.reset_work_after_bundle_rejection(&wi_id, "validation failure")`

The `InReview → Ready` transition uses the override table (work.rs:134-139), which is allowed for the `coordinator` role. The Integrator uses `"role": "coordinator"` here because this is a lifecycle recovery action — the integrator is acting as the coordinator's delegate to unstick the pipeline. The `"override": true` flag is already supported by `handle_work_transition` (handlers.rs:1405) and tells the handler to check the override transition table instead of the normal table.

**Edge case: concurrent rejections.** If multiple bundles for the same Work are rejected in the same cycle (e.g., merge conflict rejects all bundles in a batch), the first `InReview → Ready` succeeds and subsequent calls are no-ops (Work is already `Ready`, the transition `Ready → Ready` is invalid but the Integrator logs a warning and continues). This is harmless.

**Edge case: Work already terminal.** If the Work was abandoned by the coordinator (via SLA timeout) before the Integrator processes the rejection, the transition fails. The Integrator logs a warning and continues. The Work is already in a terminal state, so no recovery is needed.

### Fix 3: Reverse State Summary Section Order

The state summary currently builds sections top-down: Plans → Specs → Phases → Works → Bundles → Ticks → Agents → Locks. When truncated, the tail (Works, Bundles, Agents) gets dropped — exactly the information the coordinator needs most.

**Fix:** Reverse the section order so the most actionable information comes first:

```
Works → Bundles → Agents → Ticks → Locks → Phases → Specs → Plans
```

When `truncate_prose()` drops the tail, it now drops Plans/Specs (static, rarely changing) instead of Works/Agents (dynamic, always relevant).

This is a single change in `build_state_summary_with_sla()`: reorder the section-building blocks. No logic changes, no new functions — just move the code blocks.

Additionally, filter completed/terminal items from the summary — there's no reason to show `Done` Works or `Merged` Bundles in the state summary since they're no longer actionable.

### Architecture

No new modules or dependencies. All changes are within existing files:

| File | Change |
|------|--------|
| `src/agents/implementer.rs` | Add `strip_markdown_fences()`, call it in `parse_actions()` |
| `src/agents/integrator.rs` | Add `reset_work_after_bundle_rejection()`, call after each rejection |
| `src/agents/coordinator.rs` | Reorder sections in `build_state_summary_with_sla()` |

### Implementation Plan

1. **Add `strip_markdown_fences()` and wire into `parse_actions()`** — Add the function, apply it before `normalize_action_keys`, add tests for fenced and unfenced inputs.
2. **Add `reset_work_after_bundle_rejection()` in the Integrator** — Add the helper method, call it in all three rejection paths (stale, merge conflict, validation failure), add tests.
3. **Reorder state summary sections** — Move the Works/Bundles/Agents blocks before Plans/Specs/Phases in `build_state_summary_with_sla()`, add tests verifying Works appear before Plans in output.

## Alternatives Considered

### Alternative A (Fix 1): Regex-based fence stripping

- **Description:** Use a regex like `(?s)^\s*```\w*\n(.*?)```\s*$` to strip fences.
- **Pros:** Handles edge cases like nested fences.
- **Cons:** Adds a regex dependency, more complex than needed for this case.
- **Why not chosen:** The simple `strip_prefix`/`strip_suffix` approach handles the observed pattern. LLMs don't produce nested fences.

### Alternative B (Fix 2): Coordinator listens for `bundle.stale_replan_needed` event

- **Description:** Add an event listener in the coordinator's FSM loop that resets Work status when it receives the stale replan event.
- **Pros:** Follows event-driven architecture.
- **Cons:** The coordinator's broadcast receiver currently only handles cancellation events. Adding event processing to the FSM loop increases complexity. The integrator already knows the Work ID and can reset it directly.
- **Why not chosen:** Direct action at the rejection site is simpler and more reliable than async event processing.

### Alternative C (Fix 3): Increase token budget

- **Description:** Raise the state_summary budget from 3000 to 6000 tokens.
- **Pros:** One-line change.
- **Cons:** Doubles the context consumed by state summary, reducing room for hierarchy, learnings, and previous summary. Doesn't scale — more Works will eventually overflow any static budget.
- **Why not chosen:** The root cause is section ordering, not budget size.

### Alternative D (Fix 3): Rolling window with item count cap

- **Description:** Only show the N most recent items per section.
- **Pros:** Guarantees bounded size.
- **Cons:** The coordinator needs to see ALL non-terminal Works to make correct decisions. Capping to N could hide blocked or stuck Works.
- **Why not chosen:** Filtering terminal items + reordering is sufficient. The coordinator only needs non-terminal items, which are naturally bounded by phase size.

## Technical Considerations

### Dependencies

- No new external dependencies
- `src/agents/implementer.rs` — Fix 1
- `src/agents/integrator.rs` — Fix 2
- `src/agents/coordinator.rs` — Fix 3

### Performance

All fixes are negligible:
- Fix 1: One string scan per LLM response (~14 chars typical)
- Fix 2: One IPC call + one learning creation per rejection (already doing bundle rejection IPC)
- Fix 3: Same data, different order — zero additional computation

### Testing Strategy

**Fix 1:**
- Test: `strip_markdown_fences` with ```` ```json\n[]\n``` ```` → `[]`
- Test: `strip_markdown_fences` with bare `[]` → `[]` (no-op)
- Test: `strip_markdown_fences` with prose + fenced JSON → strips only outer fences
- Test: `parse_actions` with fenced empty array returns `Ok(vec![])`

**Fix 2:**
- Test: After stale bundle rejection, Work status is `Ready`
- Test: After merge conflict rejection, Work status is `Ready`
- Test: A Learning is created with the rejection reason
- Test: If Work transition fails (already terminal), it logs a warning but doesn't panic

**Fix 3:**
- Test: Works section appears before Plans section in output
- Test: Terminal items (Done Works, Merged Bundles) are excluded
- Test: Summary stays within 3000-token budget for a phase with 6 Works

### Rollout Plan

Direct code changes. No feature flags — these are bug fixes for deterministic behavior.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Fence stripping removes valid content inside fences | Low | Medium | Only strips outer fences from the full response. Inner content is preserved. Tests cover edge cases. |
| Work reset to Ready triggers infinite retry loop | Medium | Medium | Existing `max_work_retries` guard in coordinator (line ~1293) caps attempts. The Learning provides context for the next attempt. |
| Integrator using "coordinator" role for Work transition | Low | Low | The override table explicitly allows `InReview → Ready` for coordinator role. This is a lifecycle management action, not a domain violation. |
| Reordering summary breaks coordinator prompt expectations | Low | Low | The coordinator prompt doesn't depend on section order. It reads whatever state is present. |

## Open Questions

- [x] ~~Should `strip_markdown_fences` handle nested fences?~~ No — LLMs don't produce nested fences in action responses. Simple prefix/suffix stripping is sufficient.
- [x] ~~Should the Integrator use its own role for the Work transition?~~ No — `InReview → Ready` is only in the override table for the coordinator role. Using "coordinator" role is correct for this recovery action.
- [x] ~~Should terminal items be filtered from ALL summary sections or just Works/Bundles?~~ All sections already filter non-terminal items. The "Recently Merged Bundles" section should be kept — it shows bundles merged since the last sweep iteration, providing visibility into the Integrated → Done pipeline.

## References

- `src/agents/implementer.rs:68-110` — `parse_actions()` function
- `src/agents/integrator.rs:297-386` — Stale bundle rejection logic
- `src/agents/integrator.rs:519-556` — Merge conflict rejection
- `src/agents/integrator.rs:700-745` — Validation failure rejection
- `src/agents/coordinator.rs:47-265` — `build_state_summary_with_sla()`
- `src/agents/context.rs:136-203` — `TokenBudget` configuration
- `src/agents/context.rs:75-127` — Truncation functions
- `src/domain/work.rs:113-153` — Override transitions
