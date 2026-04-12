# Design Document: v4 Decomposer as Strategy

**Author:** Scott A. Idler
**Date:** 2026-04-11
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

This document expresses v3's full and brief decomposition pipelines as YAML strategy definitions, proving the composition engine's expressiveness. The decomposer ceases to be a standalone Rust module - it becomes a configurable pipeline of strategies that chain `classify-tier`, `decompose`, `validate-document`, `evaluate-coverage`, and `ratify-hierarchy` primitives. New decomposition strategies (3-level, iterative refinement, depth-first) become YAML changes, not Rust changes.

## Problem Statement

### Background

v3's decomposer (`src/decomposer.rs`, ~900 lines) is a monolithic async function that:
1. Classifies the plan as brief or full (tier-gate LLM call)
2. Decomposes Plan -> Specs (sequential LLM call)
3. Decomposes Specs -> Phases (parallel per spec)
4. Decomposes Phases -> Works (parallel per phase)
5. Validates each child (per-child LLM call)
6. Detects dependency cycles (topological sort)
7. Resolves dependencies (title-to-ID mapping)
8. Ratifies the hierarchy (bottom-up LLM validation)
9. Persists everything to stores and filesystem

This is the most complex single operation in Loopr. It's also the one AR most wants to experiment with - alternative decomposition depths, different validation strategies, iterative refinement loops, dependency patterns.

### Problem

The decomposition pipeline is hardcoded. Brief mode skips Specs and Phases; full mode always does Plan -> Spec -> Phase -> Work. There's no way to:
- Try a 3-level pipeline (Plan -> Phase -> Work, skipping Spec)
- Try iterative deepening (decompose one level, evaluate, then decide whether to go deeper)
- Change validation from per-child to per-batch
- Make ratification blocking instead of advisory
- Skip validation entirely for speed

Every one of these experiments requires modifying `decomposer.rs`.

### Goals

- Express v3's full decomposition (Plan -> Spec -> Phase -> Work) as a YAML pipeline
- Express v3's brief decomposition (Plan -> Work) as a YAML pipeline
- Enable new decomposition strategies expressible as YAML changes:
  - 3-level (Plan -> Phase -> Work)
  - 5-level (Plan -> Epic -> Spec -> Phase -> Work) using existing domain types
  - Iterative refinement (decompose, evaluate coverage, re-decompose gaps)
- Configurable validation: per-child, per-batch, blocking, advisory, or disabled
- Configurable ratification: blocking, advisory, or disabled
- Dependency pattern selection: sequential-chain, fan-out, explicit
- The v3 decomposer module becomes unnecessary once these strategies work

### Non-Goals

- New domain types beyond Plan/Spec/Phase/Work (decided in vision doc: type set is fixed)
- Changing what the `decompose` primitive does internally (LLM call, parsing, cycle detection)
- Real-time decomposition (strategies fire and complete; LLM calls happen inside primitives)
- GUI for pipeline editing

## Proposed Solution

### Overview

The decomposer is a standard agent that performs single-level decomposition: read an Active parent, create Pending children. The multi-level pipeline (Plan -> Spec -> Phase -> Work) is driven entirely by the composition engine's FSM and reconciliation strategies - not by a pipeline executor inside the agent.

### Decomposition Config Schema

Instead of a pipeline YAML with stages, the decomposer role config specifies per-parent-kind behavior:

```yaml
# strategies/roles/decomposer.yml
decomposer:
  model: claude-sonnet-4-6
  max-tokens: 4096
  temperature: 0.3

  # Per-parent-kind decomposition rules.
  # The engine fires decompose-when-active for any Active parent with no children.
  # This config tells the decomposer agent WHAT to create.
  rules:
    plan:
      target-kind: spec                    # full mode: Plan -> Specs
      prompt: decompose/spec.pmt
      count-guidance: 1-3
      dependency-pattern: sequential-chain
    spec:
      target-kind: phase
      prompt: decompose/phase.pmt
      count-guidance: 1-5
      dependency-pattern: sequential-chain
    phase:
      target-kind: work
      prompt: decompose/work.pmt
      count-guidance: 1-5
      dependency-pattern: fan-out
```

Brief mode is a simple override of the plan rule's target-kind:

```yaml
# strategies/roles/decomposer-brief.yml
decomposer:
  model: claude-sonnet-4-6
  max-tokens: 4096
  temperature: 0.3
  rules:
    plan:
      target-kind: work                    # brief mode: Plan -> Works directly
      prompt: decompose/work.pmt
      count-guidance: 1-8
      dependency-pattern: fan-out
    # No spec or phase rules - the engine never fires decompose-when-active
    # for Specs or Phases because they don't exist in brief mode.
```

The tier-gate strategy selects which decomposer role config to use:

```yaml
# strategies/decomposition/classify.yml
classify-and-configure:
  trigger: plan-accepted
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

### Decomposition Rule Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `target-kind` | string | (required) | Domain type to decompose into (spec, phase, work) |
| `prompt` | string | (required) | Prompt .pmt file path or inline content (principle 10) |
| `count-guidance` | string | none | Soft guidance for the LLM ("1-3", "1-5", "1-8") |
| `dependency-pattern` | string | fan-out | How children depend on each other |

### Dependency Patterns

| Pattern | Meaning | When to use |
|---------|---------|-------------|
| `fan-out` | Children have no dependencies on each other | Works that can execute independently |
| `sequential-chain` | Each child depends on the previous one (A -> B -> C) | Phases that must execute in order |
| `explicit` | Dependencies are whatever the LLM declares | When the LLM should decide dependency structure |

### How the Engine Drives Decomposition

**Key insight: the FSM hierarchy IS the pipeline.** There is no separate pipeline executor or background workflow engine. The decomposer is a standard single-level agent: it reads one Active parent and creates Pending children. The multi-level pipeline emerges from the composition engine reacting to FSM state changes via the existing reconciliation strategies from Doc 5.

**One strategy, one agent capability:**

```yaml
# strategies/decomposition/default.yml
decompose-when-active:
  description: Decompose any Active parent that has no children yet
  trigger: parent-active-no-children    # state-query: record is Active AND has no children
  scope: plan                           # also fires for spec, phase via scope aliasing
  priority: 900
  action:
    - primitive: spawn-agent
      guard: no-active-sessions         # don't spawn if already decomposing
      params:
        role: decomposer
        target-id: $trigger.scope-id
```

**The decomposer agent does one thing:** reads the Active parent, calls the LLM once, creates Pending children, terminates. It does not loop, does not execute multi-stage pipelines, does not drive FSM transitions.

**The multi-level flow emerges naturally from the FSM:**

1. **Tick N:** Plan becomes Active (no children). Engine fires `decompose-when-active`. Spawns decomposer for Plan.
2. **Agent runs:** Decomposer reads Plan, calls LLM, creates Pending Specs. Terminates.
3. **Tick N+1:** Reconciliation promotes Pending Specs to Active (parent is Active, deps met).
4. **Tick N+2:** Specs are Active (no children). Same `decompose-when-active` strategy fires for each Spec. Spawns decomposers.
5. **Agents run:** Each decomposer reads its Spec, calls LLM, creates Pending Phases. Terminates.
6. **Tick N+3:** Reconciliation promotes Phases to Active.
7. **Tick N+4:** Phases are Active (no children). Same strategy fires. Spawns decomposers.
8. **Agents run:** Each decomposer creates Pending Works. Terminates.
9. **Tick N+5:** Reconciliation promotes Works to Ready. Execution begins.

**Why this is structurally superior to a pipeline executor:**

- **Zero shadow orchestrators.** The engine controls all FSM transitions. No background task usurps the composition engine's role.
- **True composability.** Brief mode = decomposer creates Works directly from Plan (skips Spec/Phase levels). Three-level = skip Spec. Change `target-kind` in the strategy, not a pipeline YAML file.
- **Crash resilience for free.** If the daemon crashes after Specs are created but before Phases exist, the Specs are in Active state with no children. On restart, the engine fires `decompose-when-active` for each Spec. No "resume from partial state" logic needed.
- **Validation and ratification are strategies, not agent internals.** Validation fires as a separate strategy when children are created. Ratification fires when all children at a level are validated. These are composable, not hardcoded inside an agent.

**Level-triggered fallback (principle 8):**
- `parent-active-no-children`: "record is Active with zero children in child collection" - catches restarts, agent crashes, any state where decomposition should have happened but didn't.

**Alternative considered and rejected: pipeline.yml with background agent executor.** The initial design (prior revision of this doc) had the decomposer agent reading a pipeline YAML and executing stages internally. This was rejected in architectural review as a "shadow orchestrator" - a procedural workflow engine hidden inside spawn-agent, violating the single-tick constraint. The Implementer agent's internal tool loop is different because it operates within a single Work item and doesn't create FSM-managed entities. The decomposer creates Specs, Phases, and Works - entities the composition engine must manage. Embedding that control flow in an agent bypasses the engine.

### Expressing v3's Exact Behavior

v3's decomposition flow maps to the pipeline as follows:

| v3 Step | Pipeline Element | Config |
|---------|-----------------|--------|
| classify_brief() | Pre-pipeline: `classify-tier` primitive selects which pipeline to run | trigger: doc-accepted |
| Plan -> Specs (sequential) | Stage `specs` with parallel=false | Sequential LLM calls |
| Specs -> Phases (parallel per spec) | Stage `phases` with parallel=true | join_all across specs |
| Phases -> Works (parallel per phase) | Stage `works` with parallel=true | try_join_all across phases |
| Per-child validation (advisory) | validation.enabled=true, blocking=false | Warning only |
| Cycle detection | Inside `decompose` primitive (always runs) | Not configurable - safety invariant |
| Dependency resolution | Inside `decompose` primitive (always runs) | Not configurable - safety invariant |
| Ratification (advisory) | ratification.enabled=true, blocking=false | Warning only |
| Partial failure handling | on-partial-failure=continue | Persist successes, surface error |

### New Pipelines AR Could Explore

```yaml
# strategies/decomposition/three-level.yml
three-level:
  description: Plan -> Phase -> Work (skip Spec for smaller plans)
  stages:
    phases:
      parent-kind: plan
      child-kind: phase
      prompt: decompose/phase.pmt
      count-guidance: 2-6
      dependency-pattern: sequential-chain
      parallel: false
    works:
      parent-kind: phase
      child-kind: work
      prompt: decompose/work.pmt
      count-guidance: 1-5
      dependency-pattern: fan-out
      parallel: true
  validation:
    enabled: true
    per-child: true
    blocking: false
  ratification:
    enabled: true
    blocking: false
  on-partial-failure: continue
```

```yaml
# strategies/decomposition/strict.yml
strict:
  description: Full pipeline with blocking validation and ratification
  stages:
    specs:
      parent-kind: plan
      child-kind: spec
      prompt: decompose/spec.pmt
      count-guidance: 1-3
      dependency-pattern: sequential-chain
      parallel: false
    phases:
      parent-kind: spec
      child-kind: phase
      prompt: decompose/phase.pmt
      count-guidance: 1-5
      dependency-pattern: sequential-chain
      parallel: true
    works:
      parent-kind: phase
      child-kind: work
      prompt: decompose/work.pmt
      count-guidance: 1-5
      dependency-pattern: fan-out
      parallel: true
  validation:
    enabled: true
    per-child: true
    blocking: true                         # validation failures BLOCK decomposition
  ratification:
    enabled: true
    blocking: true                         # ratification failures BLOCK execution
  on-partial-failure: fail                 # any branch failure = total failure
```

```yaml
# strategies/decomposition/iterative.yml
iterative:
  description: Decompose one level, evaluate coverage, re-decompose gaps
  stages:
    specs:
      parent-kind: plan
      child-kind: spec
      prompt: decompose/spec.pmt
      count-guidance: 1-3
      dependency-pattern: explicit
      parallel: false
  validation:
    enabled: true
    per-child: true
    blocking: false
  ratification:
    enabled: false
  on-partial-failure: continue
  # After initial decomposition, coverage evaluation triggers re-decompose
  # if gaps are found. This is handled by the existing coverage trigger +
  # re-decompose primitive (Doc 2), not by the pipeline itself.
  # The pipeline decomposes ONE level; the engine's trigger system handles iteration.
```

### Tier-Gate as Pipeline Selector

The tier-gate classification determines which pipeline runs. This is a strategy, not part of the pipeline:

```yaml
# strategies/decomposition/classify.yml
classify-and-decompose:
  description: Classify plan as brief or full, then trigger appropriate pipeline
  trigger: plan-accepted                  # event: plan transitioned to Active
  scope: plan
  priority: 900
  action:
    - name: classify
      primitive: classify-tier
      params:
        plan-id: $trigger.scope-id
    - name: dispatch
      primitive: emit-event
      params:
        event-type: decomposition.start
        payload:
          plan-id: $trigger.scope-id
          pipeline: $context.classify.tier   # "brief" or "full"
```

The engine has a built-in mapping: `pipeline: brief` -> load `strategies/decomposition/brief.yml`, `pipeline: full` -> load `strategies/decomposition/full.yml`. AR can add new pipelines and new tier-gate classifications.

### Coverage-Driven Re-Decomposition

v3's re-decomposition (ReviseParent + re-decompose) becomes a strategy chain:

```yaml
# strategies/decomposition/coverage-loop.yml
evaluate-coverage-after-decompose:
  description: After decomposition completes, evaluate coverage
  trigger: decomposition-completed
  scope: plan
  priority: 800
  action:
    - primitive: evaluate-coverage
      params:
        parent-collection: plan
        parent-id: $trigger.scope-id

re-decompose-on-gaps:
  description: If coverage evaluation finds gaps, re-decompose
  trigger: coverage-incomplete
  scope: plan
  priority: 790
  action:
    - primitive: re-decompose
      params:
        parent-id: $trigger.scope-id
        parent-collection: plan
        target-kind: spec
        reason: coverage-gaps-detected
        preserve-ids: $trigger.event.adequate-children
```

This is a loop driven by events: decompose -> evaluate -> re-decompose (if gaps) -> evaluate -> ... until coverage is complete or attempt limit is reached (via the `decomposition-attempt-limit` threshold trigger from Doc 4).

### Implementation Plan

#### Phase 1: Pipeline Schema and Parser

1. Define `DecompositionPipeline` struct with stages, validation, ratification config
2. Parse pipeline YAML from `strategies/decomposition/`
3. Startup validation: stage parent/child kind consistency, prompt paths exist, dependency patterns valid
4. Unit tests: parse valid/invalid pipelines

#### Phase 2: Pipeline Execution

1. Engine generates strategies from pipeline definitions at startup
2. Implement stage execution: sequential and parallel decomposition
3. Implement validation and ratification as configurable steps
4. Wire stage completion events to next stage triggers
5. Integration tests: run full and brief pipelines against test data

#### Phase 3: v3 Equivalence

1. Write `full.yml` and `brief.yml` pipeline definitions
2. Run the same E2E decomposition scenarios as v3
3. Verify identical hierarchy output (same structure, same dependency patterns)
4. Verify identical error handling (partial failure, cycle detection, unresolved deps)

#### Phase 4: New Pipelines

1. Write `three-level.yml`, `strict.yml`, `iterative.yml`
2. AR trial tests: score each pipeline against the same target repo
3. Verify pipeline switching via tier-gate works end-to-end

## Alternatives Considered

### Alternative 1: Keep decomposer as a single "mega-primitive"

- **Description:** The entire decomposition is one primitive: `decompose-hierarchy(plan_id, mode)`. The pipeline is inside the primitive, not in YAML.
- **Pros:** Simple. No pipeline schema needed. Same code as v3.
- **Cons:** AR can only choose "brief" or "full" - can't experiment with pipeline structure. Defeats the purpose of v4.
- **Why not chosen:** The whole point is making the pipeline composable. A mega-primitive hides the composition.

### Alternative 2: Each stage as a separate strategy (no pipeline abstraction)

- **Description:** Instead of a pipeline YAML, each stage is an independent strategy wired by events. No "pipeline" concept at all.
- **Pros:** Maximum flexibility. Each stage is independently configurable.
- **Cons:** Authoring a decomposition pipeline requires writing 5+ strategies with correct event wiring. Easy to get wrong. Hard to see the pipeline at a glance.
- **Why not chosen:** The pipeline abstraction is valuable - it makes the sequence explicit and generates the strategies automatically. Under the hood it's still strategies; the pipeline is sugar that prevents wiring errors.

### Alternative 3: Pipeline as a DAG (not a sequence)

- **Description:** Stages form a DAG with explicit dependencies, allowing non-linear pipelines.
- **Pros:** Could express "decompose specs and validate them before decomposing phases."
- **Cons:** Overkill for decomposition, which is inherently hierarchical (parent before children). A DAG engine adds complexity for a use case that's always a pipeline. Validation and ratification are already configurable per-stage.
- **Why not chosen:** Pipelines are sufficient. If a non-linear flow is ever needed, it can be expressed as strategy chains.

## Technical Considerations

### Dependencies

- **Internal:** `decompose` primitive (Doc 2), `validate-document` primitive, `evaluate-coverage` primitive, `ratify-hierarchy` primitive, `classify-tier` primitive, composition engine (Doc 5)
- **External:** None new. Pipeline YAML is parsed with serde_yaml.

### Performance

- Pipeline parsing happens once at startup.
- Decomposition runs in a spawned async task, not in the engine tick. No tick stalls.
- LLM calls dominate runtime (seconds each). Parallel stages use bounded concurrency (`buffer_unordered(N)` where N defaults to 4) to avoid rate-limit exhaustion. Not unbounded `join_all`.
- Pipeline config fields (`count-guidance`, `dependency-pattern`) are passed to the `decompose` primitive via the agent task's pipeline config, not as direct primitive params. The decomposer agent reads pipeline config and constructs the appropriate prompt and dependency resolution strategy internally.

### Security

- Pipeline definitions are loaded from `strategies/decomposition/`, not arbitrary paths.
- The `prompt` field references prompt templates, not arbitrary files.

### Testing Strategy

- **Pipeline parser tests:** Valid/invalid pipeline YAML, stage consistency checks.
- **Stage execution tests:** Mock the `decompose` primitive, verify stages fire in order with correct params.
- **v3 equivalence tests:** Run full and brief pipelines, compare output hierarchy structure to v3.
- **New pipeline tests:** Run three-level and strict pipelines, verify they produce valid hierarchies.
- **Coverage loop tests:** Verify re-decompose fires on incomplete coverage, stops on complete or attempt limit.

### Rollout Plan

- Implement on v4 branch
- Phase 1-2 depend on the composition engine (Doc 5) being implemented
- Phase 3 is the validation gate: v3 equivalence must pass
- Phase 4 (new pipelines) is the payoff: AR can experiment with decomposition strategies

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Pipeline abstraction can't express v3's exact parallel/sequential behavior | Low | High | The stages table explicitly maps v3's behavior. Sequential and parallel are configurable per stage. |
| Coverage-driven re-decomposition loop doesn't converge | Medium | Medium | Existing `decomposition-attempt-limit` threshold trigger (from Doc 4) caps iterations. Same safety net as v3. |
| New pipelines produce invalid hierarchies | Medium | Medium | Cycle detection and dependency resolution are inside the `decompose` primitive - they run regardless of pipeline config. Structural safety is guaranteed. |
| Pipeline schema too rigid for unforeseen decomposition patterns | Medium | Low | The pipeline is sugar over strategies. If the schema can't express something, fall back to raw strategies (Alternative 2). |

## Resolved Questions

- [x] **Fixed domain type set?** Yes (decided in vision doc). Pipelines compose from Plan/Spec/Phase/Work. A "5-level" pipeline reuses these types with creative naming.
- [x] **Cycle detection configurable?** No. It's a safety invariant inside the `decompose` primitive. Always runs.
- [x] **Where does tier-gate live?** It's a strategy that fires before the pipeline, not part of the pipeline itself. It selects which pipeline to run.

## Open Questions

- [ ] Should the pipeline support conditional stages? e.g., "only decompose into phases if the spec has more than 3 acceptance criteria." This approaches scripting territory but could be useful. Current answer: no, use the tier-gate to select different pipelines instead.
- [ ] How does the engine map pipeline name to pipeline file? Convention (filename = pipeline name) or explicit registry?

## References

- `docs/v4-vision.md` - v4 architecture vision, decomposition pipeline schema
- `docs/design/2026-04-11-primitive-vocabulary.md` - decomposition primitives (Doc 2)
- `docs/design/2026-04-11-strategy-composition.md` - composition engine (Doc 5)
- `src/decomposer.rs` - v3 decomposer (~900 lines being replaced)
- `src/daemon/handlers/doc.rs` - v3 decomposition entry point and persistence
- `src/agents/coordinator/run.rs` - v3 coordinator interaction with decomposer
