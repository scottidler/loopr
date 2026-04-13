# Design Document: v4 Decomposer as Strategy

**Author:** Scott A. Idler
**Date:** 2026-04-11
**Status:** Implemented (Phases 1-3 complete; Phase 4 E2E tests scaffolded but ignored pending decomposer.decompose handler)
**Review Passes Completed:** 5/5 + Architect review fixes

## Summary

This document expresses v3's decomposition logic as standard composition engine strategies and a single-level decomposer agent, proving that the engine from Docs 2-5 is sufficient to drive multi-level hierarchy construction without any new orchestration layer. The v3 decomposer ceases to be a monolithic pipeline executor and becomes a thin single-level agent: read one Active parent, create Pending children, terminate. The multi-level flow (Plan -> Spec -> Phase -> Work) emerges from the FSM and the existing reconciliation strategies already deployed in Doc 5. New decomposition shapes are role config changes, not code changes.

## Problem Statement

### Background

v3's decomposer (`src/decomposer.rs`, ~2,200 lines) is a monolithic async function that orchestrates a multi-level LLM call sequence: classify tier, decompose Plan to Specs, Specs to Phases in parallel, Phases to Works in parallel, validate each child, detect dependency cycles, resolve dependency titles to IDs, and ratify the hierarchy. It is the most complex single operation in Loopr and the one AR most wants to experiment with.

Doc 5 (v0.1.127) replaced the coordinator loop, integrator cycle, and supervisor with the composition engine. The decomposer was explicitly left out of Doc 5 scope. Doc 6 closes that gap.

### Problem

The decomposer is a shadow orchestrator: it creates FSM-managed entities (Specs, Phases, Works) outside the composition engine's control. This creates four concrete problems:

1. **Decomposition depth is hardcoded.** Full mode is always Plan -> Spec -> Phase -> Work. Brief mode is always Plan -> Work. Trying a 3-level pipeline requires modifying `decomposer.rs`.
2. **Validation and ratification are hardcoded.** You cannot disable them, make them blocking, or change per-child to per-batch without code changes.
3. **The engine cannot react to partial decomposition.** If the daemon crashes after Specs exist but before Phases are created, there is no engine trigger to resume. The old decomposer is re-invoked from the coordinator, not the engine.
4. **AR cannot experiment with decomposition structure.** AR can tune temperatures and retry counts but cannot change the pipeline shape.

### Goals

- Express v3's full decomposition (Plan -> Spec -> Phase -> Work) using the existing composition engine and a single-level decomposer agent
- Express v3's brief decomposition (Plan -> Work) as a role config change, not a different code path
- Enable new decomposition shapes (3-level, iterative) as YAML config changes with no Rust changes
- Make validation and ratification configurable: disabled, advisory, or blocking
- Make decomposition crash-resilient: daemon restart resumes from current FSM state with no special recovery code
- Delete `src/decomposer.rs` as a pipeline executor once strategies drive the full flow

### Non-Goals

- New domain types beyond Plan/Spec/Phase/Work (type set is fixed per vision doc)
- Changing what the `decompose` primitive does internally (LLM call, parsing, cycle detection, dependency resolution)
- Real-time streaming of decomposition progress to TUI
- GUI for decomposition config editing

## Proposed Solution

### Overview

**The FSM hierarchy IS the pipeline.** There is no pipeline executor. Multi-level decomposition emerges from three things already present in the system after Doc 5:

1. **A `has-no-children` state query** added to `StateQueryRegistry` (one new built-in, one function)
2. **Level-specific composite triggers** combining `parent-active` with `has-no-children` for each hierarchy level
3. **Three `decompose-when-active` strategies** (one per scope: plan, spec, phase) that spawn a single-level decomposer agent

The decomposer agent is a standard single-level agent: reads one Active parent, calls the LLM once, creates Pending children, terminates. The engine's existing reconciliation strategies promote Pending children to Active on the next tick, triggering the next level's decomposer. The multi-level flow is not programmed - it falls out of the FSM.

### New State Query

One new built-in is added to `StateQueryRegistry` in `src/trigger/observe.rs`:

```
has-no-children: true when the scoped record has zero children in the specified child collection
```

This is the complement of the existing `has-children` query. Implementation: `ctx.children(id, child_col).is_empty()`.

### New Triggers

Six new triggers are added. Three check whether a record is itself Active (self-scoped). Three check whether a record has no children at the expected next level. Composites pair them.

**Self-Active state queries** added to `strategies/triggers/reconciliation.yml`:

```yaml
# Each record checks its own status field, not its parent's.
# This is distinct from parent-active (which checks upward) and
# phase-parent-active (which is phase-scoped). Plans have no parent,
# so they cannot use parent-active - they must check themselves.

plan-is-active:
  type: state-query
  scope: plan
  query: field-equals
  params:
    field: status
    value: Active

spec-is-active:
  type: state-query
  scope: spec
  query: field-equals
  params:
    field: status
    value: Active

phase-is-active:
  type: state-query
  scope: phase
  query: field-equals
  params:
    field: status
    value: Active
```

**No-children state queries** added to `strategies/triggers/reconciliation.yml`:

```yaml
plan-active-no-specs:
  type: state-query
  scope: plan
  query: has-no-children
  params:
    child-collection: spec

spec-active-no-phases:
  type: state-query
  scope: spec
  query: has-no-children
  params:
    child-collection: phase

phase-active-no-works:
  type: state-query
  scope: phase
  query: has-no-children
  params:
    child-collection: work
```

**Composite triggers** added to `strategies/triggers/composites.yml`. Each fires only when the record is itself Active AND has no children yet:

```yaml
plan-decomposable:
  type: composite
  operator: and
  triggers:
    - plan-is-active         # the plan's own status is Active
    - plan-active-no-specs   # the plan has no spec children yet

spec-decomposable:
  type: composite
  operator: and
  triggers:
    - spec-is-active         # the spec's own status is Active
    - spec-active-no-phases

phase-decomposable:
  type: composite
  operator: and
  triggers:
    - phase-is-active        # the phase's own status is Active
    - phase-active-no-works
```

### Decomposition Strategies

Three strategies in `strategies/decomposition/default.yml`, one per hierarchy level:

```yaml
decompose-plan:
  description: Decompose an Active plan with no spec children
  trigger: plan-decomposable
  scope: plan
  priority: 850
  action:
    - primitive: spawn-agent
      guard: no-active-sessions
      params:
        role: decomposer
        target-id: $trigger.scope-id

decompose-spec:
  description: Decompose an Active spec with no phase children
  trigger: spec-decomposable
  scope: spec
  priority: 850
  action:
    - primitive: spawn-agent
      guard: no-active-sessions
      params:
        role: decomposer
        target-id: $trigger.scope-id

decompose-phase:
  description: Decompose an Active phase with no work children
  trigger: phase-decomposable
  scope: phase
  priority: 850
  action:
    - primitive: spawn-agent
      guard: no-active-sessions
      params:
        role: decomposer
        target-id: $trigger.scope-id
```

The `no-active-sessions` guard (already implemented in `GuardConditionRegistry`) scopes to the specific `scope-id`, preventing a second decomposer from spawning if one is already running for that parent.

### Tier Classification

A strategy fires when a plan transitions to Active, classifying it as full or brief and storing the result on the plan record:

```yaml
# strategies/decomposition/classify.yml

classify-and-configure:
  description: Classify plan tier and store decomposer config for agent lookup
  trigger: plan-approved       # existing trigger: plan.field plan-approved = true
  scope: plan
  priority: 1000
  action:
    - name: classify
      primitive: classify-tier
      params:
        plan-id: $trigger.scope-id
    - primitive: update-record
      params:
        collection: plan
        id: $trigger.scope-id
        fields:
          decomposer-config: $context.classify.tier   # "full" or "brief"
```

### Decomposer Role Config

The decomposer agent reads its role config at spawn time based on the `decomposer-config` field on the Plan. Default is `full`:

```yaml
# strategies/roles/decomposer.yml  (full mode - default)
decomposer:
  model: claude-sonnet-4-6
  max-tokens: 4096
  temperature: 0.3

  # Fallback if plan.decomposer-config is not set.
  default-config: full

  rules:
    plan:
      target-kind: spec
      prompt: decompose/spec.pmt
      count-guidance: 1-3
      dependency-pattern: sequential-chain
      validation: advisory
    spec:
      target-kind: phase
      prompt: decompose/phase.pmt
      count-guidance: 1-5
      dependency-pattern: sequential-chain
      validation: advisory
    phase:
      target-kind: work
      prompt: decompose/work.pmt
      count-guidance: 1-5
      dependency-pattern: fan-out
      validation: advisory
```

Brief mode is a config variant with no code changes required:

```yaml
# strategies/roles/decomposer-brief.yml  (brief mode)
decomposer:
  model: claude-sonnet-4-6
  max-tokens: 4096
  temperature: 0.3

  rules:
    plan:
      target-kind: work          # Plan -> Works directly
      prompt: decompose/work.pmt
      count-guidance: 1-8
      dependency-pattern: fan-out
      validation: advisory
    # No spec or phase rules. Those levels never become Active in brief mode
    # because the plan decomposer creates Works, not Specs or Phases.
```

AR can define a `three-level` config variant that maps `plan` to `phase` (skipping Spec) without writing any Rust.

### Decomposer Agent

The decomposer agent (`src/agents/decomposer/`) is a standard single-level agent structured identically to existing agents:

1. Receive `target-id` from spawn params
2. Read the target record (Plan, Spec, or Phase) from stores
3. Look up the plan's `decomposer-config` field (walk up to the plan from any level)
4. Load the role config file `strategies/roles/decomposer-{config}.yml`
5. Find the rule matching the parent kind; if no rule exists, emit `decomposition.failed` and terminate
6. Call the `decompose` primitive with `parent-id`, `target-kind`, `count-guidance`, `dependency-pattern`. The `decompose` primitive owns all child record creation - it calls the LLM, parses output, detects cycles, resolves dependency titles to IDs, and persists Pending child records to stores. The agent receives back a child count.
7. **If the primitive returns zero children:** call `transition-record` to move the parent to `Complete` with reason `no-children-generated`, then terminate. This prevents the `*-decomposable` trigger from re-firing on the next tick (a parent with status Complete will not match `*-is-active`).
8. If `validation: advisory` or `validation: blocking`, call `validate-document` for each created child
9. Emit `decomposition.completed` on success, `decomposition.failed` on error
10. Terminate

The agent does not loop. It does not orchestrate multiple levels. It does not manage FSM transitions beyond the zero-children edge case above. It is a thin wrapper around the `decompose` primitive that applies role config.

### How Multi-Level Flow Emerges

The full decomposition across engine ticks. Agents run asynchronously between ticks; the `no-active-sessions` guard prevents re-spawn while an agent is running. Children appear in stores only after the agent completes - the engine sees them on the next tick.

| Tick | State | Engine action |
|------|-------|---------------|
| N | Plan is Active, no Spec children | `plan-decomposable` fires. `decompose-plan` spawns decomposer agent. |
| N..N+k | Decomposer agent runs asynchronously | Reads Plan, calls LLM, `decompose` primitive persists Pending Specs. Agent emits `decomposition.completed`, terminates. |
| N+k | Specs are Pending, Plan is Active | Reconciliation promotes Specs to Active (`promote-pending-specs` strategy). |
| N+k | Each Spec is Active, no Phase children | `spec-decomposable` fires per Spec. `decompose-spec` spawns one decomposer per Spec (parallel). |
| N+k..N+m | Decomposer agents run asynchronously | Each creates Pending Phases via `decompose` primitive, terminates. |
| N+m | Phases promoted to Active by reconciliation | |
| N+m | Each Phase is Active, no Work children | `phase-decomposable` fires per Phase. `decompose-phase` spawns decomposers. |
| N+m..N+p | Decomposers create Pending Works | Reconciliation promotes Works to Ready. Execution begins. |

Brief mode: the plan decomposer creates Works directly. Specs and Phases never exist. `spec-decomposable` and `phase-decomposable` never fire. No code path difference.

### Crash Resilience

If the daemon crashes after Specs are created (Active) but before Phases exist: on restart, the engine evaluates all triggers against current state. `spec-decomposable` fires for each Active Spec with no Phase children. Decomposers spawn. No recovery code, no special IPC handler, no "resume decomposition" operation.

**Atomicity requirement:** The `decomposer.decompose` IPC handler must write all generated children via a `create_many` batch call to `TaskStore`. Either all children land in the JSONL log or none do. This is what makes the crash resilience claim true: `has-no-children` always reflects a clean state on restart - there is no partial-children scenario where some Phases exist but others do not, which would permanently strand the trigger.

### Validation and Ratification

These are optional strategies enabled or disabled independently:

```yaml
# strategies/decomposition/validate.yml

validate-after-decomposition:
  description: Validate children after each decomposition level completes
  trigger: decomposition-completed    # existing event trigger in agent-events.yml
  scope: plan
  priority: 800
  action:
    - primitive: validate-document
      params:
        collection: $trigger.event.child-collection
        id: $trigger.event.child-id

ratify-spec-level:
  description: Ratify hierarchy when all specs are validated
  trigger: spec-children-terminal    # existing trigger from reconciliation.yml
  scope: plan
  priority: 790
  action:
    - primitive: ratify-hierarchy
      params:
        parent-id: $trigger.scope-id
```

To disable validation: set `validate-after-decomposition` to `enabled: false`. To make validation blocking: add a `validation-passed` guard to the `promote-pending-specs` strategy. These are YAML changes with no Rust required.

### Coverage-Driven Re-Decomposition

The `coverage-incomplete` trigger already defined in `strategies/triggers/reconciliation.yml` drives re-decomposition:

```yaml
# strategies/decomposition/coverage.yml

re-decompose-on-gaps:
  description: Re-decompose when coverage evaluation finds gaps
  trigger: coverage-incomplete      # existing trigger
  scope: plan
  priority: 780
  action:
    - primitive: re-decompose
      params:
        parent-id: $trigger.scope-id
        parent-collection: plan
        target-kind: spec
        reason: coverage-gaps-detected
        preserve-ids: $trigger.event.adequate-children
```

The `decomposition-attempt-limit` threshold trigger caps re-decomposition iterations. It is defined here (not in Doc 4 - v3 incremented the counter but never enforced the limit; this is the first enforced definition):

```yaml
# strategies/triggers/reconciliation.yml

decomposition-attempt-limit:
  type: state-query
  scope: plan
  query: field-exceeds-threshold
  params:
    field: decomposition-attempts
    threshold-config-key: max-decomposition-attempts
```

This fires when `plan.decomposition-attempts` exceeds the `max-decomposition-attempts` config value (default: 3). The `re-decompose` primitive increments `decomposition-attempts` each time it runs. When the trigger fires, the strategy should escalate or mark the plan as `Failed` to prevent indefinite cycling.

### Dependency Patterns

The `decompose` primitive applies the configured dependency pattern when writing child records:

| Pattern | Behavior |
|---------|---------|
| `fan-out` | Children have no dependencies on each other |
| `sequential-chain` | Each child depends on the previous (A -> B -> C) |
| `explicit` | Dependencies are whatever the LLM declares |

Pattern is specified per parent kind in the role config. No new primitive code needed - this is an existing param on `decompose`.

### Implementation Plan

#### Phase 1: New State Query and Triggers
**Model:** sonnet

1. Add `has-no-children` built-in to `StateQueryRegistry` in `src/trigger/observe.rs` (complement of `has-children`)
2. Add `plan-active-no-specs`, `spec-active-no-phases`, `phase-active-no-works` to `strategies/triggers/reconciliation.yml`
3. Add `plan-decomposable`, `spec-decomposable`, `phase-decomposable` composites to `strategies/triggers/composites.yml`
4. Unit tests: `has-no-children` with known store contents; composite trigger evaluation at each level

#### Phase 2: Strategy and Role Config YAML
**Model:** sonnet

1. Write `strategies/decomposition/default.yml` - three `decompose-*` strategies
2. Write `strategies/decomposition/classify.yml` - `classify-and-configure` strategy
3. Write `strategies/roles/decomposer.yml` - full mode role config
4. Write `strategies/roles/decomposer-brief.yml` - brief mode role config
5. Write `strategies/decomposition/validate.yml` - optional validation/ratification strategies
6. Write `strategies/decomposition/coverage.yml` - re-decomposition strategy
7. Run `otto ci` - verify all new YAML passes structural validation and registry checks

#### Phase 3: Decomposer Agent
**Model:** opus

1. Create `src/agents/decomposer/` module with `mod.rs` entry point
2. Implement spawn entry: receive `target-id`, look up parent record, walk to plan for `decomposer-config`
3. Load role config file from `strategies/roles/decomposer-{config}.yml`
4. Find matching rule for parent kind; bail with `decomposition.failed` if no rule found
5. Call `decompose` primitive with `parent-id`, `target-kind`, `count-guidance`, `dependency-pattern`
6. Apply per-child validation if `validation` is set in rule
7. Emit `decomposition.completed` (with `child-collection`, `child-id` per child) or `decomposition.failed`
8. Wire `decomposer.start` IPC handler analogous to `implementer.start` and `coordinator.start`
9. Wire `decomposer.decompose` IPC handler: this is the handler the `decompose` primitive bridges to. It contains the actual LLM call, output parsing, cycle detection, dependency resolution, and child record persistence. Children must be written via `TaskStore::create_many` (atomic batch) so that `has-no-children` always reflects a clean state on restart. Add `AgentKind::Decomposer` to `src/agents/kind.rs`.
10. Unit tests: mocked `decompose` primitive, verify rule selection, param translation, event emission

#### Phase 4: v3 Equivalence and Cleanup
**Model:** opus

1. E2E test: full decomposition via engine produces same Plan -> Spec -> Phase -> Work structure as v3
2. E2E test: brief decomposition produces same Plan -> Work structure as v3
3. Crash-resume test: kill daemon after Specs created, restart, verify Phases created on next ticks
4. Confirm `decompose_hierarchy` in `src/decomposer.rs` is no longer called from any live code path
5. Delete `src/decomposer.rs` once dead-code gate passes; delete disabled test wrappers
6. Resolve tier classification pipeline: the `classify-and-configure` strategy writes `decomposer-config` via `update-record`, but `plan.update` only persists `title` and `acceptance_criteria`. Either wire `update-record` to support arbitrary Plan fields, or remove the strategy and have the decomposer agent read `plan.tier` directly (the field already exists on the Plan struct and is set by `classify-tier`)

## Alternatives Considered

### Alternative 1: Pipeline schema as a separate abstraction layer

- **Description:** Define a `DecompositionPipeline` struct with `stages:`, `parallel:`, `parent-kind`, `child-kind`. The engine compiles pipeline YAML into strategies at startup.
- **Pros:** Makes the full Plan -> Spec -> Phase -> Work sequence explicit in one file. Prevents stage wiring errors.
- **Cons:** Introduces a domain-specific DSL on top of the generic strategy engine - a shadow orchestrator inside the engine itself. Hides composition mechanics from AR. Two unresolved open questions (pipeline name to file mapping; engine extension API). Violates vision principle 6 (composition, not scripting) and principle 7 (strategies are single-tick, flows chain via events).
- **Why not chosen:** Reviewed by the Architect and confirmed incompatible with the v4 vision. The wiring concern it addresses is a phantom: there is exactly one strategy to wire per level, and the level-by-level progression is dictated by the decomposer's role config. A pipeline compiler abstracts away the engine, defeating the goal of exposing atomic composition to AR.

### Alternative 2: Mega-primitive (keep decomposer.rs as one primitive)

- **Description:** Wrap the entire existing `decomposer.rs` as a single `decompose-hierarchy(plan_id, mode)` primitive.
- **Pros:** Minimal change. Same code as v3.
- **Cons:** AR can only choose brief/full. Cannot experiment with pipeline structure. Does not achieve YAML-composable orchestration.
- **Why not chosen:** Makes the decomposer opaque to the engine. The engine cannot observe or react to partial decomposition state.

### Alternative 3: Independent hand-wired strategies per level (no role config)

- **Description:** Write separate strategies for Plan->Spec, Spec->Phase, Phase->Work with explicit event wiring between them, no role config abstraction.
- **Pros:** Maximum transparency.
- **Cons:** Authoring a new decomposition shape requires writing multiple strategy files and getting event wiring correct. Role config provides the right abstraction: it configures what the agent does at each level without adding orchestration machinery.
- **Why not chosen:** Role config is simpler and consistent with how other agent roles work. Changing decomposition depth becomes a config edit, not a strategy authoring exercise.

## Technical Considerations

### Dependencies

- **Internal:** `classify-tier`, `decompose`, `validate-document`, `ratify-hierarchy`, `re-decompose`, `spawn-agent` primitives (all in Doc 2 catalog, all implemented at v0.1.127). `has-children` state query (implemented). `no-active-sessions` guard (implemented). Composition engine (Doc 5, v0.1.127).
- **New:** `has-no-children` state query (one built-in function addition to `StateQueryRegistry`).
- **External:** None new.

### Performance

- Each decomposition level is one agent invocation (one LLM call). The multi-level flow takes depth ticks with the engine's active interval (5s) between levels. This is acceptable for a one-time setup operation.
- Sibling nodes at the same level decompose independently and in parallel. Three Specs decomposing simultaneously are three independent agent tasks with no coordination overhead.
- The `no-active-sessions` guard adds one store read per strategy evaluation - negligible.

### Security

- The decomposer agent reads role config from `strategies/roles/` - a path under version control.
- The `decomposer-config` field on the Plan record is set by the `classify-and-configure` strategy, not by external input. It cannot be injected via IPC.

### Testing Strategy

- **Unit tests (Phase 1):** `has-no-children` with various store states. Composite trigger evaluation against known store contents.
- **Unit tests (Phase 3):** Decomposer agent with mocked `decompose` primitive. Verify rule selection by parent kind, correct params passed, correct events emitted. Cover: plan with no config (default to full), unknown parent kind (bail with failed event), validation rule applied.
- **E2E tests (Phase 4):** Full and brief decomposition flows. Crash-resume at each level boundary. v3 output structure equivalence.

### Rollout Plan

- Phases 1-3 on v4 branch; Phase 4 is the gate before deleting `src/decomposer.rs`
- `src/decomposer.rs` is deleted only when Phase 4 E2E tests pass - not before

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Sibling decomposer race: two agents decomposing the same parent | Low | Medium | `no-active-sessions` guard on `spawn-agent` scopes to the specific `scope-id`. Atomic session creation prevents double-spawn. |
| Zero children returned by LLM - infinite re-fire loop | Medium | High | Agent transitions parent to `Complete` when `decompose` returns zero children. A Complete record does not match `*-is-active`, so the `*-decomposable` trigger never fires again for that parent. |
| Stranded decomposer session (hard crash without session cleanup) | Low | Medium | `no-active-sessions` guard remains true indefinitely, stalling that branch. Existing SLA/timeout strategies (`work-sla-breach`, `goal-timeout`) surface this via escalation. Phase 3 unit tests cover session cleanup on agent failure. |
| Partially created children after agent crash | Low | Low | Children are written atomically via `create_many` batch call in the `decomposer.decompose` handler - either all land or none do. On restart, `has-no-children` evaluates to true (no children) and the decomposer re-spawns cleanly. A hard power failure mid-flush is the only residual risk; the `coverage-incomplete` trigger handles downstream detection if it occurs. |
| `decomposer-config` not set on plan (classify strategy missed) | Low | Low | Role config declares `default-config: full`. Agent falls back to full mode if field is absent. |
| `decompose` primitive params don't align with role config fields | Medium | Medium | Phase 3 unit tests verify param translation for all three levels. |
| `src/decomposer.rs` is still called from a live path after Phase 4 | Medium | High | Phase 4 dead-code check: compile with `#[deny(dead_code)]` on the module, verify no callers before deletion. |
| Coverage re-decomposition loop diverges | Low | Low | `decomposition-attempt-limit` threshold trigger (defined in Coverage-Driven Re-Decomposition section above) caps iterations. |

## Resolved Questions

- **Scope aliasing:** Three separate strategy instances (one per scope) rather than one strategy with multi-scope support. Three instances are explicit and consistent with the composite trigger pattern from Doc 5.
- **Level-specific vs generic triggers:** Level-specific composites (`plan-decomposable`, `spec-decomposable`, `phase-decomposable`) rather than a single `parent-active-no-children` trigger. Level-specific is explicit, avoids requiring the engine to know the expected child collection per scope, and is consistent with the existing pattern.
- **Pipeline abstraction vs emergence:** Full emergence. No `DecompositionPipeline` struct, no pipeline compiler. Confirmed by Architect review as consistent with v4 vision principles 6 and 7.

## References

- `docs/v4-vision.md` - v4 architecture vision; principles 6 (composition not scripting) and 7 (single-tick strategies, flows chain via events)
- `docs/design/2026-04-11-primitive-vocabulary.md` - Doc 2: `decompose`, `classify-tier`, `validate-document`, `ratify-hierarchy`, `re-decompose`, `spawn-agent`
- `docs/design/2026-04-11-fsm-in-yaml.md` - Doc 3: FSM Active/Pending/Complete states, reconciliation
- `docs/design/2026-04-11-trigger-guard-system.md` - Doc 4: `has-children`, `no-active-sessions`, `coverage-incomplete`
- `docs/design/2026-04-11-strategy-composition.md` - Doc 5: composition engine, `promote-pending-specs/phases/works` strategies, `decomposition-completed` event trigger
- `src/decomposer.rs` - v3 monolithic decomposer being replaced
- `src/primitive/catalog/decompose.rs` - existing decompose primitive implementations
- `src/trigger/observe.rs` - `StateQueryRegistry` with `has-children` built-in
- `strategies/triggers/reconciliation.yml` - existing state-query triggers including `coverage-incomplete`
- `strategies/triggers/composites.yml` - existing composite triggers
- `strategies/triggers/agent-events.yml` - existing `decomposition-completed` and `decomposition-failed` event triggers
