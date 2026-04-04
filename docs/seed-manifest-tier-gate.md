# seed_manifest and the Tier Gate

## What seed_manifest is

`seed_manifest` is a shortcut activation path for e2e tests (and any caller passing a `.yml`/`.yaml` file as the plan). Instead of the interactive coordinator flow (interview → LLM generates hierarchy → coordinator approves each level), it takes a pre-written YAML manifest and bulk-inserts the entire Plan/Spec/Phase/Work hierarchy in one shot.

**Caller:** `cli/dispatch.rs` - when the user runs `loopr run --plan some.yml`, the CLI detects the `.yml` extension and fires `coordinator.seed_manifest` instead of the normal `set_goal` + `accept_plan` flow.

## Execution tree

```
loopr run --plan manifest.yml
  └─ cli/dispatch.rs: detects .yml → coordinator.seed_manifest
       └─ handle_coordinator_seed_manifest()
            └─ parse_manifest() → resolved{Plan, Spec, Phases, Works}
            └─ force_activate Plan, Spec, Phases
            └─ insert Works (already Ready)
            └─ CoordinatorState(InterviewMode::Skip, plan_approved=true)
            └─ coordinator agent wakes up in Planning FSM state
```

The normal interactive path:

```
loopr run --goal "..." --plan description.txt
  └─ set_goal
  └─ coordinator LLM: interview → CreatePlan → ActivatePlan ← tier gate lives here
       └─ CreateSpec → ActivateSpec
       └─ CreatePhase → ActivatePhase
       └─ CreateWork ...
```

## The bug

`seed_manifest` entirely bypasses `handle_coordinator_activate_plan`. The tier gate (`classify_tier` via Haiku LLM call) is wired into `activate_plan` only. `seed_manifest` never touches it. Every e2e run gets `tier=Full` silently by default, regardless of plan content.

This was discovered during an e2e run for `python-api`. No tier-related log entries appeared at all - because the classification never ran.

## The premature fix (DO NOT SHIP as-is)

A `classify_tier` call was added directly into `seed_manifest` before the plan is persisted. This makes the LLM call happen on the e2e path too. But it was added without discussion and may be wrong.

## The real question

**Should `seed_manifest` even be calling `classify_tier`?**

The manifest already has a fully-specified hierarchy: Spec, Phases, and Works are all present and seeded. Brief mode means Plan→Work directly - no Spec, no Phases. If the manifest includes Specs and Phases, the tier is already implicit in its shape: it's Full.

Classifying a manifest that already has Spec+Phase+Work as "Brief" would be a contradiction - the Spec and Phases would already be in the store with no coordinator logic to drive them.

### Options to discuss

1. **Don't classify - infer from manifest shape.** If `resolved.spec` is present, tier=Full. If works have `parent_id` pointing directly to the plan, tier=Brief. No LLM call needed.

2. **Classify but only as a sanity check.** Run `classify_tier` but only log a warning if it disagrees with the manifest shape. Don't override.

3. **Remove the fix and accept that seed_manifest is always Full.** e2e tests use manifests; manifests are always Full. Brief mode only applies to the interactive path. Document this explicitly.

4. **Brief manifests don't exist yet.** Brief mode was designed for the interactive path. Manifests may never be Brief. Defer the question until Brief manifests are actually needed.

## Current state of code

The premature fix is committed in `src/daemon/handlers/coordinator.rs` around line 499. It adds the full `TierGateConfig` → `LlmClient` → `classify_tier` call inline in `seed_manifest`, duplicating the logic from `activate_plan`.

If the answer is "infer from shape" or "seed_manifest is always Full", this code should be reverted.
