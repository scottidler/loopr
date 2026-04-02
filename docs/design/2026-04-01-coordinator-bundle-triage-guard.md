# Design Document: Coordinator Bundle Triage Guard

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 3/3

## Summary

The Coordinator LLM calls `triage_bundle` on Reviewed bundles, triggering an invalid FSM transition (Reviewed->Triaged) that kills the Coordinator via Lifeguard after 3 repeats. The fix: split the state summary's "Bundles (actionable)" section into status-specific subsections with explicit action hints, and add pre-flight status guards to `triage_bundle` and `accept_bundle`.

## Problem Statement

### Background

The Coordinator's `build_state_summary` method (coordinator.rs:104-121) presents bundles to the LLM in a section labeled "Bundles (actionable)". This section includes both `Proposed` and `Reviewed` bundles with no distinction between them:

```rust
.filter(|b| matches!(b.status, BundleStatus::Proposed | BundleStatus::Reviewed))
```

The output looks like:
```
### Bundles (actionable)
- [bd-abc] Proposed (wi: wk-123)
- [bd-def] Reviewed (wi: wk-456)
```

The Coordinator prompt (coordinator.pmt:19) says:
> **Executing**: Priority order: (1) triage pending Bundles, (2) retry failed WIs, (3) wait.

The actions list (coordinator.pmt:36-37) offers two bundle actions:
```
10. triage_bundle   {"action": "triage_bundle", "bundle_id": "..."}
11. accept_bundle   {"action": "accept_bundle", "bundle_id": "..."}
```

But there is no guidance on which action applies to which bundle status.

### Problem

The LLM sees "Bundles (actionable)" containing a Reviewed bundle, reads the instruction "triage pending Bundles", and calls `triage_bundle` on the Reviewed bundle. The FSM correctly rejects `Reviewed -> Triaged` (only `Proposed -> Triaged` is valid). The `triage_bundle` handler (executor.rs:1675-1696) has no pre-flight status check - it blindly fires the transition request and returns `ActionError` when the FSM rejects it.

The LLM retries the same call. The Lifeguard detects 3 identical errors and kills the Coordinator. A new Coordinator is spawned, sees the same state summary, and may repeat the cycle.

Observed in lua-todo E2E (v0.1.52): Coordinator ag-8s0na died with:
```
lifeguard: tool validation loop: same error repeated 3 times:
triage_bundle failed: transition rejected: invalid transition
from Reviewed to Triaged for role coordinator
```

### Valid Coordinator actions on bundles by status

| Current Status | Coordinator Action | Shown in Summary? |
|---------------|-------------------|-------------------|
| Proposed | `triage_bundle` (->Triaged) | Yes - "Bundles (actionable)" |
| Triaged | (none - Reviewer handles this) | No |
| Reviewed | `accept_bundle` (->Accepted) | Yes - "Bundles (actionable)" |
| Accepted+ | (none - Integrator handles this) | No |

Both Proposed and Reviewed appear under the same "actionable" header with no action hint. The LLM has no way to know which action applies to which status.

### Goals

- Eliminate the Coordinator death loop caused by triaging Reviewed bundles
- Make the state summary unambiguous about which action each bundle needs
- Add defense-in-depth guards so invalid triage/accept calls fail gracefully with a helpful message

### Non-Goals

- Changing the FSM transition rules themselves
- Restructuring the Coordinator prompt (one-line tweak to Executing priorities is in scope)
- Adding new bundle actions (Supersede is handled via the generic `transition` action)

## Proposed Solution

### Overview

Two layers of defense:

1. **State summary clarity** - Split "Bundles (actionable)" into status-specific subsections with explicit action hints so the LLM never has to guess which action to use.
2. **Action handler guards** - Add pre-flight status checks to `triage_bundle` and `accept_bundle` that return clear, corrective error messages instead of opaque FSM rejection errors.

### Layer 1: State summary (coordinator.rs:104-121)

**Before:**
```
### Bundles (actionable)
- [bd-abc] Proposed (wi: wk-123)
- [bd-def] Reviewed (wi: wk-456)
```

**After:**
```
### Proposed Bundles (use triage_bundle)
- [bd-abc] Proposed (wi: wk-123)

### Reviewed Bundles (use accept_bundle)
- [bd-def] Reviewed (wi: wk-456)
```

Split the single filter into two blocks:
1. Filter `BundleStatus::Proposed` - header: "Proposed Bundles (use triage_bundle)"
2. Filter `BundleStatus::Reviewed` - header: "Reviewed Bundles (use accept_bundle)"

Each section only appears if there are matching bundles (same pattern as existing code).

### Layer 1b: Prompt reinforcement (coordinator.pmt:19)

**Before:**
```
- **Executing**: Priority order: (1) triage pending Bundles, (2) retry failed WIs, (3) wait.
```

**After:**
```
- **Executing**: Priority order: (1) triage Proposed Bundles, (2) accept Reviewed Bundles, (3) retry failed WIs, (4) wait.
```

This reinforces the structural cues in the state summary. The LLM sees "triage Proposed" in the prompt AND "Proposed Bundles (use triage_bundle)" in the data.

### Layer 2: Action handler guards (executor.rs:1675-1696)

**triage_bundle - add status pre-flight:**

After the `bd-` prefix check and before the bridge request, read the bundle's current status. If it's not `Proposed`, return a corrective `ActionError`:

```rust
AgentAction::TriageBundle { bundle_id } => {
    // existing bd- prefix check...

    let bundles = bridge.stores().read_bundles()?;
    match bundles.get(&bundle_id) {
        None => {
            return Ok(ActionResult::ActionError(format!(
                "triage_bundle: bundle {} not found", bundle_id
            )));
        }
        Some(bundle) if bundle.status != BundleStatus::Proposed => {
            return Ok(ActionResult::ActionError(format!(
                "triage_bundle: bundle {} is {} not Proposed. {}",
                bundle_id, bundle.status,
                match bundle.status {
                    BundleStatus::Reviewed => "Use accept_bundle instead.",
                    _ => "No triage action needed.",
                }
            )));
        }
        _ => {}
    }

    // existing bridge.request(...)
}
```

**accept_bundle - add status pre-flight:**

Same pattern. If the bundle is not `Triaged` or `Reviewed`, return a corrective error:

```rust
AgentAction::AcceptBundle { bundle_id } => {
    // existing bd- prefix check...

    let bundles = bridge.stores().read_bundles()?;
    match bundles.get(&bundle_id) {
        None => {
            return Ok(ActionResult::ActionError(format!(
                "accept_bundle: bundle {} not found", bundle_id
            )));
        }
        Some(bundle) if !matches!(bundle.status, BundleStatus::Triaged | BundleStatus::Reviewed) => {
            return Ok(ActionResult::ActionError(format!(
                "accept_bundle: bundle {} is {} not Triaged/Reviewed. {}",
                bundle_id, bundle.status,
                match bundle.status {
                    BundleStatus::Proposed => "Use triage_bundle first.",
                    _ => "No accept action needed.",
                }
            )));
        }
        _ => {}
    }

    // existing bridge.request(...)
}
```

### Files Changed

| File | Change |
|------|--------|
| `src/agents/coordinator.rs` | Split "Bundles (actionable)" filter into Proposed and Reviewed subsections (~lines 104-121) |
| `src/agents/executor.rs` | Add status guard to `triage_bundle` (~line 1681) and `accept_bundle` (~line 1703) |
| `prompts/coordinator.pmt` | Update Executing priority to distinguish triage vs accept (line 19) |

### Implementation Plan

Single phase - this is a small, focused change.

1. Split the bundle filter in `build_state_summary_with_sla`
2. Add pre-flight guards to both action handlers
3. Update existing `test_build_state_summary_with_bundles` test to check for the new section headers
4. Add tests: triage_bundle on Reviewed bundle returns corrective ActionError; accept_bundle on Proposed bundle returns corrective ActionError
5. Run `otto ci`

## Alternatives Considered

### Alternative 1: Remove Reviewed bundles from the summary entirely

- **Description:** Filter to `Proposed` only. The Coordinator never sees Reviewed bundles.
- **Pros:** Simplest change. LLM can't triage what it can't see.
- **Cons:** The Coordinator has a valid action on Reviewed bundles (`accept_bundle`). Hiding them means the Coordinator can't accept bundles that the Reviewer approved, stalling the pipeline until the Integrator auto-accepts (if it does).
- **Why not chosen:** The Coordinator needs to see Reviewed bundles to advance them. The problem isn't visibility - it's the missing action hint.

### Alternative 2: Only fix the prompt, not the state summary

- **Description:** Add text to coordinator.pmt explaining when to use triage vs accept.
- **Pros:** No code change to the summary builder.
- **Cons:** The LLM still sees an undifferentiated list. Prompt instructions are weaker than structural cues in the data. The LLM is more likely to follow a section header "use triage_bundle" than a paragraph it read 2000 tokens ago.
- **Why not chosen:** Structural cues in the data are more reliable than prose instructions. Both layers together are strongest.

### Alternative 3: Merge triage_bundle and accept_bundle into a single action

- **Description:** One `advance_bundle` action that auto-detects the right transition based on current status.
- **Pros:** LLM can't call the wrong one.
- **Cons:** Conflates two semantically different decisions (triaging = "this bundle is worth reviewing" vs accepting = "this reviewed bundle should be merged"). Reduces the Coordinator's ability to reject at the triage stage.
- **Why not chosen:** The two actions represent different decisions. Merging them loses granularity.

## Testing Strategy

1. **State summary tests:** Existing `test_build_state_summary_with_bundles` - update to check for "Proposed Bundles" and "Reviewed Bundles" section headers separately.
2. **Guard tests:** New tests in executor:
   - `test_triage_bundle_rejects_reviewed_bundle` - create a Reviewed bundle, call triage_bundle, assert ActionError with "use accept_bundle" hint
   - `test_accept_bundle_rejects_proposed_bundle` - create a Proposed bundle, call accept_bundle, assert ActionError with "use triage_bundle" hint
   - `test_triage_bundle_succeeds_on_proposed` - happy path still works
   - `test_accept_bundle_succeeds_on_reviewed` - happy path still works

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM ignores section headers and still calls wrong action | Low | Low | Layer 2 guard catches it with a corrective message that feeds back via ActionError |
| Pre-flight read adds latency to every triage/accept call | Low | Low | Single hashmap lookup, negligible cost |
| Existing tests depend on "Bundles (actionable)" header string | Med | Low | Update the test assertions |

## Open Questions

None.

## References

- lua-todo E2E v0.1.52: Coordinator ag-8s0na killed by Lifeguard after Reviewed->Triaged loop
- BundleStatus FSM: `src/domain/bundle.rs:10-30`
- Coordinator prompt: `prompts/coordinator.pmt`
