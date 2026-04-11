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

A decomposition pipeline is a YAML file that declares a sequence of stages. Each stage decomposes a parent type into a child type using the `decompose` primitive, with configurable validation and dependency patterns. The composition engine executes the pipeline by firing strategies for each stage in sequence, connected by events.

### Decomposition Pipeline Schema

```yaml
# strategies/decomposition/full.yml
full:
  description: Plan -> Spec -> Phase -> Work (v3 default)
  # Stage ordering is inferred from the parent-child chain (plan -> spec -> phase -> work),
  # NOT from YAML document order. The engine topologically sorts stages by parent-kind/child-kind
  # relationships. Keyed map form gives O(1) lookup and duplicate detection.
  stages:
    specs:
      parent-kind: plan
      child-kind: spec
      prompt: decompose/spec
      count-guidance: 1-3
      dependency-pattern: sequential-chain
      parallel: false                      # specs decomposed sequentially
    phases:
      parent-kind: spec
      child-kind: phase
      prompt: decompose/phase
      count-guidance: 1-5
      dependency-pattern: sequential-chain
      parallel: true                       # phases decomposed in parallel across specs
    works:
      parent-kind: phase
      child-kind: work
      prompt: decompose/work
      count-guidance: 1-5
      dependency-pattern: fan-out
      parallel: true                       # works decomposed in parallel across phases
  validation:
    enabled: true
    per-child: true
    blocking: false                        # v3: validation is advisory (warning only)
  ratification:
    enabled: true
    blocking: false                        # v3: ratification is advisory
  on-partial-failure: continue             # v3: persist successful branches, surface error
```

```yaml
# strategies/decomposition/brief.yml
brief:
  description: Plan -> Work (skip Spec and Phase)
  stages:
    works:
      parent-kind: plan
      child-kind: work
      prompt: decompose/work
      count-guidance: 1-8
      dependency-pattern: fan-out
      parallel: false
  validation:
    enabled: true
    per-child: true
    blocking: false
  ratification:
    enabled: false                         # v3: brief mode skips ratification
  on-partial-failure: fail                 # brief has one stage - partial = total failure
```

### Pipeline Stage Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `parent-kind` | string | (required) | Domain type of the parent record (plan, spec, phase) |
| `child-kind` | string | (required) | Domain type to decompose into (spec, phase, work) |
| `prompt` | string | (required) | Prompt template path (relative to prompts directory) |
| `count-guidance` | string | none | Soft guidance for the LLM ("1-3", "1-5", "1-8") |
| `dependency-pattern` | string | fan-out | How children depend on each other |
| `parallel` | bool | false | Whether children of different parents at this level are decomposed in parallel |

### Dependency Patterns

| Pattern | Meaning | When to use |
|---------|---------|-------------|
| `fan-out` | Children have no dependencies on each other | Works that can execute independently |
| `sequential-chain` | Each child depends on the previous one (A -> B -> C) | Phases that must execute in order |
| `explicit` | Dependencies are whatever the LLM declares | When the LLM should decide dependency structure |

### How the Engine Executes a Pipeline

The pipeline definition is not itself a strategy - it's a **configuration** that the engine uses to generate strategies at startup. Each stage becomes one or more strategies wired by events:

**Stage 1 (Specs):**
```
Trigger: plan-ready-for-decomposition (plan status = Active, no children)
Action: decompose(plan_id, target=spec, prompt=decompose/spec)
Event on completion: stage.specs.completed
```

**Stage 2 (Phases):**
```
Trigger: stage.specs.completed
Action: for each spec, decompose(spec_id, target=phase, prompt=decompose/phase)
  (parallel if parallel=true)
Event on completion: stage.phases.completed
```

**Stage 3 (Works):**
```
Trigger: stage.phases.completed
Action: for each phase, decompose(phase_id, target=work, prompt=decompose/work)
  (parallel if parallel=true)
Event on completion: stage.works.completed
```

**Validation (if enabled):**
```
Trigger: each stage completion
Action: for each child, validate-document(child_id)
  (blocking or advisory per config)
```

**Ratification (if enabled):**
```
Trigger: all stages completed (stage.works.completed for full, stage.works.completed for brief)
Action: ratify-hierarchy(plan_id)
  (blocking or advisory per config)
```

**Completion:**
```
Trigger: ratification completed (or skipped if disabled)
Action: emit decomposition.completed event
  Coordinator wakes from Decomposing state
```

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
      prompt: decompose/phase
      count-guidance: 2-6
      dependency-pattern: sequential-chain
      parallel: false
    works:
      parent-kind: phase
      child-kind: work
      prompt: decompose/work
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
      prompt: decompose/spec
      count-guidance: 1-3
      dependency-pattern: sequential-chain
      parallel: false
    phases:
      parent-kind: spec
      child-kind: phase
      prompt: decompose/phase
      count-guidance: 1-5
      dependency-pattern: sequential-chain
      parallel: true
    works:
      parent-kind: phase
      child-kind: work
      prompt: decompose/work
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
      prompt: decompose/spec
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
- Decomposition performance is dominated by LLM calls (seconds each), not engine overhead.
- Parallel stages use tokio::join_all, same as v3. No performance regression.

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
