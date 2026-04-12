# Design Document: v4 Vision - YAML-Composable Orchestration Engine

**Author:** Scott A. Idler
**Date:** 2026-04-11
**Status:** Living Document (vision/roadmap - not an implementation design doc)
**Review Passes Completed:** 5/5

## Summary

Loopr v4 replaces v3's procedural Rust orchestration with a YAML-composable engine. Rust provides a runtime of primitives (atomic operations), a generic FSM interpreter, and a composition engine. YAML defines FSM shapes, transition guards, recovery strategies, agent roles, decomposition pipelines, and scoring policies. New orchestration behaviors become YAML changes - not Rust changes. This enables AutoResearch (AR) to compose and score entirely new strategies without writing code.

## Problem Statement

### Background

Loopr v3 is a working orchestrator with a proven FSM, agent roles, decomposer, recovery safety nets, and an agentic tool loop. It has shipped through v0.1.121 with ~120 hardcoded behavioral parameters (see `docs/hardcoded-knobs-inventory.md`). The AR config surface work (v0.1.119-121) exposed LLM parameters and prompt paths as YAML - but this only covers the "field values" layer.

The deeper problem is structural: v3's orchestration logic is procedural Rust. The coordinator's `run.rs` is a state machine with hardcoded transitions. Recovery strategies are baked into handlers. The decomposer has a fixed 4-level hierarchy. Adding a new behavior (e.g., "ask a friend" when an implementer gets stuck) requires writing Rust, adding FSM transitions, and wiring handlers.

### Problem

Every orchestration policy change requires a Rust code change, recompile, and redeploy. This creates two bottlenecks:

1. **AR can only sweep numeric parameters** - it can tune temperatures, timeouts, and retry counts, but it cannot explore structural changes like new recovery strategies, alternative decomposition pipelines, or different agent collaboration patterns.

2. **Behavioral experimentation is slow** - each new idea requires design doc, implementation, compile, test, deploy. Ideas that should take minutes to express take days to ship.

### Goals

- YAML-defined FSMs with states, transitions, terminal states, and guards
- YAML-defined strategies that wire triggers to actions via composable primitives
- YAML-defined agent roles with capabilities, constraints, and tool sets
- YAML-defined decomposition pipelines (not just the current fixed 4-level hierarchy)
- YAML-defined recovery policies (composable, not hardcoded)
- YAML-defined scoring and quality gates
- v3 behavior expressible entirely in YAML (the first strategy is the current behavior)
- Startup validation that catches invalid YAML compositions before any work starts
- AR can generate, load, and score novel strategy compositions without Rust changes

### Non-Goals

- Turing-complete scripting in YAML (this is a composition engine, not a programming language)
- Hot-reloading strategies mid-run (strategies are loaded at startup, fixed for the run)
- Backward compatibility with v3 config format (clean break, v4 branch)
- GUI or web UI for strategy editing (YAML files, edited by hand or AR)
- Plugin system for external Rust code (primitives are built-in, not loadable)

## Proposed Solution

### Overview

The architecture splits into two layers:

**Runtime layer (Rust)** - stable, well-tested, rarely changed:
- Primitive registry: atomic operations (spawn-agent, retry-work, abandon, merge, inject-context, etc.)
- FSM interpreter: generic state machine that enforces transition rules from YAML definitions
- Composition engine: reads strategy YAML, evaluates triggers, fires actions, manages lifecycle
- Infrastructure: IPC, TUI, TaskStore, worktree management, LLM client, tools

**Strategy layer (YAML)** - the experimentation surface:
- FSM definitions: states, transitions, terminal states per domain type
- Role definitions: agent capabilities, model, pool size, iterations, tool set, prompts
- Strategy compositions: trigger-action-wiring chains for recovery, escalation, collaboration
- Decomposition pipelines: how plans break down (pluggable, not fixed hierarchy)
- Policies: timeouts, SLAs, queue priority formulas, scoring weights

### Key Concepts

**Primitives** are the atomic operations the runtime can perform. Each primitive has typed inputs, outputs, and preconditions. Examples: `spawn-agent`, `inject-context`, `retry-work`, `abandon-work`, `escalate`, `merge-worktree`, `evaluate-ac`, `compact-context`. Primitives are Rust functions registered by name in a global registry. YAML references them by name.

**Triggers** are conditions that fire actions. They observe the runtime state and evaluate to true/false. Examples: `consecutive-failures >= 3`, `wall-clock > 1800s`, `abandon-ratio > 0.4`, `fsm-transition(work, running -> reviewing)`, `event(decomposition.completed)`. Triggers can compose with `and`, `or`, `not`.

**Guards** are conditions on FSM transitions. A guard must evaluate to true before a transition is allowed. Example: a guard on `reviewing -> completed` might require `all-ac-passing`. Guards reference primitives or state queries.

**Strategies** are named compositions of triggers and actions. A strategy defines: when to activate (trigger), what to do (sequence of primitives), and what happens next (on-success, on-failure wiring). Strategies can reference other strategies, enabling layered composition.

**Critical constraint: strategies are single-tick.** A strategy fires, executes its action sequence, and completes - all within one engine tick. Strategies do not span ticks, do not suspend and resume, and do not hold intermediate state across engine cycles. This is the most important architectural decision in v4 and the one that keeps the system simple.

Why this matters: `spawn-agent` is a primitive that returns immediately (it starts a tokio task and hands back a session ID). The agent then runs for minutes or hours. When the agent finishes, it emits an event. That event fires a *different* strategy. Multi-step flows are expressed as chains of strategies connected by events, not as multi-step strategies that pause and wait.

This means v4 is NOT a durable workflow engine (Temporal, AWS Step Functions). There is no in-flight strategy state to persist, no resume-after-crash logic, no saga rollback. If the daemon crashes mid-strategy, the partial effects are visible in TaskStore (because primitives persist as they go), and the next engine tick will evaluate triggers against the current state and fire whatever strategies match. The system is self-healing through its reactive trigger model, not through durable execution replay.

This single-tick constraint is the firewall against accidentally building a YAML programming language. Without it, strategies would need variables, conditionals, loops, suspend/resume, error recovery branches - the full complexity of a workflow engine. With it, strategies are flat sequences of primitives with a simple success/failure fork. Complexity lives in how strategies chain via events, not in how individual strategies execute.

**The FSM interpreter** replaces `#[derive(Fsm)]`. It loads FSM definitions from YAML at startup, validates them (no orphan states, no unreachable states, terminals have no outgoing transitions), and enforces transitions at runtime. The coordinator no longer contains FSM logic - it asks the interpreter "is this transition valid for this role?" and the interpreter consults the YAML definition.

v3's FSM has two transition types that the YAML schema must preserve:
- **transitions** - normal state changes (the happy path)
- **overrides** - emergency/recovery transitions that bypass normal flow (e.g., coordinator can force a work item from InProgress back to Ready)

Both carry role authorization (who can trigger them). The YAML schema captures this with `by: [role]` on each transition target.

### Architecture

```
                         +-------------------+
                         |      Daemon       |
                         | (startup, IPC,    |
                         |  event broadcast) |
                         +---------+---------+
                                   |
                                   | owns
                                   v
+------------------+     +-------------------+     +------------------+
|                  |     |                   |     |                  |
|   YAML Strategy  |---->|  Composition      |---->|   Primitive      |
|   Definitions    |     |  Engine           |     |   Registry       |
|                  |     |                   |     |                  |
+------------------+     +-------------------+     +------------------+
                               |       |
                               v       v
                    +----------+     +------------+
                    |  FSM     |     |  Trigger   |
                    |  Interp. |     |  Evaluator |
                    +----------+     +------------+
                         |                |
                         v                v
              +-------------------------------+
              |        Runtime State           |
              |  (TaskStore, Sessions, Events) |
              +-------------------------------+
```

**Ownership chain:** The daemon starts up, loads all YAML strategy definitions, validates
them, and constructs the composition engine. The composition engine replaces v3's
coordinator as the central decision-maker. The daemon feeds events into the engine
(IPC messages, FSM transitions, timer ticks). The engine evaluates triggers, fires
strategies, and calls primitives. Primitives interact with runtime state (TaskStore,
agent sessions, worktrees) to produce side effects.

There is no separate "coordinator process" in v4. The composition engine IS the
coordinator - but its logic comes from YAML strategies instead of hardcoded Rust.

On each tick, the composition engine:
1. Collects pending events from the daemon's event bus
2. Evaluates all active triggers against current state
3. For each fired trigger, looks up the associated strategy
4. Executes the strategy's action sequence (calling primitives)
5. Processes on-success/on-failure wiring
6. Persists any state changes via TaskStore

### YAML Schema (illustrative, not final)

**FSM definition:**
```yaml
# strategies/fsm/work.yml
name: work
states:
  - draft
  - pending
  - ready
  - in-progress
  - blocked
  - in-review
  - integrated
  - done
  - abandoned
terminal:
  - done
  - abandoned
transitions:
  # Source state is the outer key, target state is the inner key.
  # "by" lists authorized roles; omit for any-role.
  draft:
    pending: { by: [coordinator] }
    ready: { by: [coordinator] }
    abandoned: { by: [coordinator] }
  pending:
    ready: { by: [coordinator] }
    abandoned: { by: [coordinator] }
  ready:
    in-progress: { by: [coordinator] }
    blocked: { by: [coordinator] }
    abandoned: { by: [coordinator] }
    done: { by: [coordinator] }
  in-progress:
    blocked: { by: [coordinator, implementer] }
    in-review: { by: [implementer] }
    abandoned: { by: [coordinator] }
  blocked:
    ready: { by: [coordinator] }
    abandoned: { by: [coordinator] }
  in-review:
    in-progress: { by: [coordinator] }
    integrated: { by: [integrator] }
    abandoned: { by: [coordinator] }
  integrated:
    done: { by: [coordinator, integrator] }
    abandoned: { by: [coordinator] }

overrides:
  in-progress:
    ready: { by: [coordinator] }
    in-review: { by: [coordinator] }
  in-review:
    ready: { by: [coordinator] }

guards:
  all-ac-passing:
    from: in-review
    to: done
    condition: all-ac-passing
```

**Role definition:**
```yaml
# strategies/roles/implementer.yml
name: implementer
model: claude-sonnet-4-6
max-tokens: 8192
temperature: 0.3
max-iterations: 20
max-pool: unlimited
session-timeout-secs: 1800
max-requeries: 3
prompt: implementer.pmt
tools:
  - read-file
  - write-file
  - edit-file
  - grep
  - glob
  - shell
  - fetch
```

**Recovery strategy:**
```yaml
# strategies/recovery/ask-a-friend.yml
name: ask-a-friend
description: When an implementer gets stuck, spawn an advisor agent with the stuck agent's context
trigger:
  condition: consecutive-failures
  scope: agent-session
  threshold: 2
action:
  - primitive: spawn-agent
    params:
      role: reviewer
      model: claude-opus-4-6
      context-from: current-session
      prompt: advisor.pmt
      max-iterations: 5
  - primitive: inject-context
    params:
      target: current-session
      source: spawned-agent-result
  - primitive: retry-work
on-failure:
  - primitive: escalate
    params:
      reason: advisor-failed
```

**v3 recovery behavior expressed as YAML (proof that the schema can capture existing logic):**
```yaml
# strategies/recovery/default.yml
# Multiple strategies in one file (YAML multi-document).
# Each strategy is independent - they could also be separate files.
# This is v3's hardcoded recovery path: retry up to 3 times, then abandon.
name: work-retry-on-failure
description: v3 default - when an agent session fails, retry the work up to max attempts
trigger:
  condition: event
  event: session-failure
  scope: work
action:
  - primitive: increment-failure-count
  - primitive: check-threshold
    params:
      field: session-failure-count
      max: 3                           # v3's max_session_failures
  - primitive: retry-work
on-failure:
  - primitive: abandon-work
    params:
      reason: max-session-failures-exceeded
---
name: work-attempt-limit
description: v3 default - hard cap on total work attempts across all failure types
trigger:
  condition: threshold
  field: attempt-count
  scope: work
  threshold: 3                         # v3's MAX_WORK_ATTEMPTS
action:
  - primitive: abandon-work
    params:
      reason: max-attempts-exceeded
---
name: abandon-ratio-escalation
description: v3 default - if too many works are abandoned, surface need-help
trigger:
  condition: ratio
  numerator: abandoned-works
  denominator: total-works
  scope: plan
  threshold: 0.4                       # v3's max_abandon_ratio
action:
  - primitive: escalate
    params:
      reason: abandon-ratio-exceeded
```

**Decomposition pipeline:**
```yaml
# strategies/decomposition/full.yml
name: full
description: Plan -> Spec -> Phase -> Work (v3 default)
stages:
  specs:
    parent-kind: plan
    child-kind: spec
    prompt: decompose/spec.pmt
    count-guidance: 1-3
    dependency-pattern: sequential-chain
  phases:
    parent-kind: spec
    child-kind: phase
    prompt: decompose/phase.pmt
    count-guidance: 1-5
    dependency-pattern: sequential-chain
    parallel-across-parents: true
  works:
    parent-kind: phase
    child-kind: work
    prompt: decompose/work.pmt
    count-guidance: 1-5
    dependency-pattern: fan-out
    parallel-across-parents: true
validation:
  per-child: true
  blocking: false
ratification:
  enabled: true
  blocking: false
```

**Policy:**
```yaml
# strategies/policies/work-sla.yml
name: work-sla
max-attempts: 3
max-wall-clock-minutes: 30
on-sla-breach:
  strategy: escalate-stuck-work
```

### Implementation Roadmap

This vision doc is the first of ~7 design docs. Each builds on the previous - we write one, implement enough to validate it, then write the next informed by what we learned.

#### Doc 1: v4 Vision (this document)
- Motivation and architectural philosophy
- The runtime/strategy split
- Carry-over vs rewrite analysis
- Directory skeleton
- Design principles
- Roadmap of subsequent docs

#### Doc 2: [Primitive Vocabulary](design/2026-04-11-primitive-vocabulary.md)
**Deliverable:** A complete catalog of every atomic operation, with the Primitive trait and registry pattern defined in Rust.
- Read v3's coordinator, executor, handlers, decomposer, and supervisor
- Extract every atomic operation the system performs
- Formalize each primitive: name, typed inputs, typed outputs, preconditions, side effects
- Define the Primitive trait and registry pattern in Rust
- Group primitives by domain: agent, work, decompose, integrate, escalate, evaluate, context
- Identify which v3 operations are truly atomic vs composed (some "primitives" may decompose further)
- After this doc: we can implement `src/primitive/` and have a working registry

#### Doc 3: [FSM-in-YAML](design/2026-04-11-fsm-in-yaml.md)
**Deliverable:** YAML schema for FSM definitions and a working runtime interpreter that passes v3's FSM test suite.
- YAML schema for FSM definitions (states, transitions, terminals, guards, role authorization, overrides)
- Runtime FSM interpreter: loads YAML, validates at startup, enforces at runtime
- How guards reference primitives or state queries
- Migration from `#[derive(Fsm)]` to runtime interpretation
- Startup validation: orphan states, unreachable states, guard references resolve
- How domain types (Plan, Spec, Phase, Work, Bundle) reference their FSM by name
- Error messages when transitions are rejected (must be as clear as v3's compile-time errors)
- After this doc: we can implement `src/fsm/` and write `strategies/fsm/*.yml` for all domain types

#### Doc 4: [Trigger and Guard System](design/2026-04-11-trigger-guard-system.md)
**Deliverable:** A trigger evaluation framework that can express every safety net in `docs/hardcoded-knobs-inventory.md`.
- Trigger types: threshold (count, ratio, time), event (FSM transition, daemon event, timer), composite (and/or/not)
- How triggers observe runtime state (what's the observation API?)
- Trigger lifecycle: armed, fired, reset
- Guard evaluation: synchronous, must be fast, cannot call LLM
- How triggers wire to strategies (the composition glue)
- Sliding windows, debouncing, cooldowns - preventing trigger storms
- After this doc: we can implement `src/trigger/` and express v3's safety nets as trigger definitions

#### Doc 5: [Strategy Composition](design/2026-04-11-strategy-composition.md)
**Deliverable:** The composition engine that wires triggers to primitives, replacing v3's coordinator loop.
- Strategy YAML schema: trigger, action sequence, on-success/on-failure wiring
- Strategy lifecycle: loaded at startup, activated when trigger scope is entered, deactivated when scope exits
- Strategy scoping: any domain collection (plan, spec, phase, work, bundle, session, tick)
- Composing strategies: one strategy's on-success can activate another
- The composition engine as the coordinator (not a hardcoded state machine)
- How v3's coordinator logic maps to strategies (the proof point)
- Conflict resolution: what happens when two strategies fire simultaneously?
- After this doc: we can implement `src/engine/` and write `strategies/recovery/default.yml`

#### Doc 6: [Decomposer as Strategy](design/2026-04-11-decomposer-as-strategy.md)
**Deliverable:** v3's full and brief decomposition expressed as YAML pipelines, proving the schema's expressiveness.
- Express v3's full and brief decomposition as YAML strategy definitions
- The decomposition pipeline as a sequence of stages, not a hardcoded function
- Pluggable validation and ratification
- New decomposition strategies AR could explore (e.g., 3-level, 5-level, iterative refinement)
- Dependency resolution as a configurable policy
- How the tier-gate classification becomes a strategy selector
- After this doc: we can write `strategies/decomposition/*.yml` and delete v3's `decomposer.rs`

#### Doc 7: [AR Trial Integration](design/2026-04-11-ar-trial-integration.md)
**Deliverable:** The trial runner that loads named strategy configs, runs orchestration, and produces comparable scores.
- Trial config format: named YAML that overrides strategy defaults
- How AR generates trial configs (parameter sweeps + structural composition)
- The scoring loop: load trial config, run orchestration, collect score, compare
- What AR can now explore that it couldn't before (structural changes, not just knob values)
- Trial reproducibility: pinning strategy versions, seed control
- Trial comparison: how to attribute score differences to specific strategy changes
- After this doc: AR can run trials that compose novel strategies and score them against v3 baseline

### Carry-Over vs Rewrite

**Carry over directly (proven, architecture-neutral):**

| Component | Why it carries over |
|-----------|-------------------|
| Domain types (Plan, Spec, Phase, Work, Bundle, Session, Tick, Lock, Chat) | Pure data, no orchestration logic |
| TaskStore integration (JSONL-as-truth, Record trait) | Persistence is orthogonal to how orchestration works |
| IPC protocol (Unix socket, framing, client/server) | Transport layer, unchanged |
| TUI (ratatui views, event loop) | Presentation layer, unchanged |
| Tools (Tool trait, all builtins) | Agent capabilities, unchanged |
| LLM client + streaming SSE | Communication layer, unchanged |
| Worktree management | Git operations, unchanged |
| CLI (clap structure, dispatch) | User interface, unchanged |
| Scorer | Evaluation, unchanged (and now more useful with strategy comparison) |

**Rewrite from scratch (the orchestration spine):**

| Component | What it becomes |
|-----------|----------------|
| Coordinator (`agents/coordinator/`) | Generic strategy interpreter driven by composition engine |
| Executor (`agents/executor/`) | Primitive sequencing driven by strategy YAML |
| Decomposer (`decomposer.rs`) | Pluggable pipeline defined in YAML |
| Supervisor (`daemon/supervisor.rs`) | Configurable restart policy from YAML |
| Recovery logic (scattered across handlers) | Composable strategies |
| Work queue (`daemon/work_queue.rs`) | Pluggable priority formula from YAML |
| Daemon handlers | Thin dispatchers that emit events into the engine |

**Build new (doesn't exist in v3):**

| Component | Purpose |
|-----------|---------|
| Composition engine | Loads strategies, evaluates triggers, fires actions |
| FSM interpreter | Generic state machine from YAML definitions |
| Primitive registry | Named atomic operations callable from YAML |
| Trigger evaluator | Condition system with composable logic |
| YAML schema + validation | Startup validation of all strategy definitions |

### Proposed Directory Skeleton

```
src/
  main.rs                    # thin shell (carry from v3)
  lib.rs                     # crate root

  # --- THE ENGINE (new) ---
  engine/
    mod.rs                   # composition engine: loads strategies, runs them
    interpreter.rs           # evaluates trigger -> action -> wiring chains
    registry.rs              # primitive registry (name -> fn)

  fsm/
    mod.rs                   # generic FSM interpreter
    schema.rs                # parses+validates FSM definitions from YAML
    runtime.rs               # runtime FSM instance: state, transitions, guards

  trigger/
    mod.rs                   # trigger trait + evaluation
    threshold.rs             # consecutive-failures, attempt-count, ratio
    event.rs                 # FSM transition, agent event, timer
    composite.rs             # and/or/not combinators

  # --- PRIMITIVES (extracted from v3 coordinator) ---
  primitive/
    mod.rs                   # Primitive trait + registry
    agent.rs                 # spawn, stop, inject-context, ask-friend
    work.rs                  # retry, abandon, block, reassign, reprioritize
    decompose.rs             # decompose, re-decompose, promote, bubble-up
    integrate.rs             # merge, rebase, resolve-conflict
    escalate.rs              # need-help, surface-error, notify
    evaluate.rs              # validate, score, gate-check
    context.rs               # build-context, compact, inject-learning

  # --- DOMAIN (carry from v3) ---
  domain/
    mod.rs
    plan.rs, spec.rs, phase.rs, work.rs
    bundle.rs, session.rs, tick.rs, lock.rs, chat.rs

  # --- AGENTS (simplified, role-agnostic runtime) ---
  agents/
    mod.rs                   # generic agent runtime (no role-specific code)
    session.rs               # session lifecycle
    pool.rs                  # worker pool
    context.rs               # context builder (carry from v3)
    llm.rs                   # LLM client, streaming (carry from v3)

  # --- TOOLS (carry from v3) ---
  tools/
    mod.rs
    loop.rs                  # agentic loop driver
    builtin/                 # read, write, edit, grep, shell, etc.

  # --- INFRASTRUCTURE (carry from v3, slim down) ---
  daemon/
    mod.rs                   # startup/shutdown
    server.rs                # event loop (delegates to engine)

  ipc/                       # carry from v3
  tui/                       # carry from v3
  worktree/                  # carry from v3
  scorer/                    # carry from v3
  config/
    mod.rs                   # YAML loading, schema validation
    schema.rs                # config struct definitions

# --- YAML DEFINITIONS (the strategy layer) ---
strategies/
  fsm/
    work.yml
    bundle.yml
    plan.yml
    spec.yml
    phase.yml
    coordinator.yml

  roles/
    coordinator.yml
    implementer.yml
    reviewer.yml
    researcher.yml
    integrator.yml

  recovery/
    default.yml              # v3 behavior expressed as YAML
    ask-a-friend.yml         # example new composition

  decomposition/
    full.yml                 # Plan -> Spec -> Phase -> Work
    brief.yml                # Plan -> Work

  scoring/
    default.yml              # 40/30/20/10 weights

  policies/
    work-sla.yml
    supervision.yml
    queue-priority.yml
```

## Alternatives Considered

### Alternative 1: Expose all knobs as config fields (keep v3 architecture)
- **Description:** Make every hardcoded value in v3 a config field with a default. AR sweeps numeric values.
- **Pros:** Minimal code change. Low risk. Fast to implement (partially done in v0.1.121).
- **Cons:** AR can only tune numbers, not explore structural changes. Adding "ask a friend" still requires Rust. The 120+ knobs become a flat, unstructured config blob.
- **Why not chosen:** This is the low-hanging fruit we already started. It doesn't solve the structural composition problem.

### Alternative 2: Lua/Wasm scripting layer
- **Description:** Embed a scripting runtime (Lua, Wasm) for orchestration logic. Strategies written in Lua.
- **Pros:** Turing-complete. Can express arbitrary logic. Hot-reloadable.
- **Cons:** Two languages to maintain. Debugging spans Rust+Lua boundary. Type safety at the boundary is painful. Overkill - we need composition, not general-purpose scripting.
- **Why not chosen:** YAML composition is sufficient for the patterns we need. A scripting layer is a bigger maintenance burden than the problem warrants.

### Alternative 3: Incremental refactor of v3 (no clean break)
- **Description:** Gradually extract primitives and add YAML interpretation on top of v3's existing coordinator.
- **Pros:** No branch, no rewrite risk. Incremental progress.
- **Cons:** The old and new systems must coexist during migration, creating confusion about which path runs. v3's module boundaries were designed around procedural orchestration - they fight the new model. Incremental rewrites of state machines tend to produce bugs at the boundary between old and new paths.
- **Why not chosen:** v2 to v3 was a clean-slate rewrite and it worked. The module structure needs to change fundamentally. A clean break is safer than a hybrid.

## Technical Considerations

### Dependencies

- **Internal:** TaskStore (unchanged), loopr-derive (may evolve or be replaced by runtime FSM)
- **External (carry over):** tokio, reqwest, ureq, ratatui, clap, serde, serde_yaml
- **External (potentially new):** a YAML schema validation crate, or hand-rolled validation

### Performance

- YAML parsing happens once at startup. Runtime cost is trigger evaluation per tick, which is lightweight (no LLM calls, no I/O - just state inspection and comparison).
- The FSM interpreter adds one HashMap lookup per transition vs v3's compiled match arm. Negligible.
- The composition engine's tick rate is configurable (same as v3's coordinator intervals).

### Security

- YAML strategies can reference primitives by name but cannot inject arbitrary code.
- The `shell` primitive (for tool execution) retains the same sandboxing as v3.
- Strategy YAML files should be committed to the repo, not loaded from arbitrary paths.

### Testing Strategy

- **Primitive tests:** Each primitive tested in isolation with injected fakes (same pattern as v3).
- **FSM interpreter tests:** Load YAML definitions, verify transition validation matches expected behavior. Port v3's FSM tests as the baseline.
- **Strategy composition tests:** Load a strategy YAML, inject mock events, verify the correct primitives fire in the correct order.
- **Integration tests:** Express v3's default behavior as YAML, run the same E2E scenarios, verify identical outcomes.
- **AR regression:** The v3-as-YAML strategy must produce comparable scores to v3 on the same target repo.

### Rollout Plan

- v4 branch, separate worktree (`~/repos/scottidler/loopr-v4`)
- v3 continues receiving bugfixes on main
- v4 development follows design doc sequence (docs 1-7)
- v4 is "done" when it can express v3's behavior in YAML and produce comparable E2E scores
- v4 replaces v3 on main when the strategy composition layer is proven

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| YAML schema can't express v3 behavior | Medium | High | Doc 6 (decomposer as strategy) is the proof point - build it early. If v3 behavior can't be expressed, the schema is wrong and we iterate. |
| Runtime validation gaps (things the compiler caught in v3) | High | Medium | Aggressive startup validation. Every strategy reference (primitive name, FSM state, trigger condition) validated before any work starts. |
| Performance regression from interpretation overhead | Low | Low | Trigger evaluation is simple state inspection. Profile if suspected. |
| Scope creep toward general-purpose scripting | Medium | High | Non-goal is explicit. If YAML can't express something, add a new primitive - don't add scripting. |
| AR generates invalid YAML compositions | Medium | Medium | Startup validation catches this. AR must validate before scoring. |
| Loss of v3 tribal knowledge during rewrite | Low | High | v3 exists as reference in adjacent worktree. Carry-over components copied, not rewritten. Design docs capture the "why" behind v3 decisions. |
| Strategy composition becomes accidentally Turing-complete | Medium | High | Design principle 6 (Greenspun's defense) + principle 7 (single-tick constraint). on-success/on-failure is the ceiling of control flow in YAML. Multi-step flows chain via events, not in-strategy logic. |
| Multi-step strategies need intermediate state management | ~~High~~ Resolved | ~~Medium~~ | Strategies are single-tick (principle 7). No intermediate state survives across ticks. Strategy-scoped context (`HashMap<String, Value>`) exists within one tick only. Multi-step flows are chains of strategies connected by events. (Doc 5) |

## Resolved Questions

All open questions from the initial vision were resolved during the design doc process (Docs 2-7):

- [x] **FSM inheritance/composition?** No. Keep flat - one YAML file per domain type. Inheritance adds resolution-order complexity for marginal DRY benefit. *(Doc 3)*
- [x] **Strategy scoping?** Scope lives on the strategy definition, not the trigger. Accepts any domain collection name (plan, spec, phase, work, bundle, session, tick, lock). *(Doc 5)*
- [x] **Primitive registry extensibility?** Strictly compiled-in. Primitives are the stability boundary. New primitive = new Rust code = new release. *(Doc 2)*
- [x] **Trigger evaluation granularity?** Both. Event triggers are push (fire on matching event). Threshold/ratio/timer/state-query triggers are pull (evaluate per tick). *(Doc 4)*
- [x] **Strategy versioning for AR?** Git. Strategy YAML files are committed. AR trial configs reference overrides against HEAD. Reproducibility comes from pinning git SHA + trial config. *(Doc 7)*
- [x] **Dynamic domain types?** No. Type set is fixed (Plan, Spec, Phase, Work, Bundle, Session, Tick, Lock, Chat). A 3-level pipeline skips Spec; it doesn't invent new types. *(Doc 6)*
- [x] **TUI display of strategy behavior?** Add an "engine" or "events" view showing fired triggers, active strategies, executed primitives. Late-stage concern - build after engine works. *(Doc 5)*
- [x] **Strategy priority/ordering?** Explicit `priority` field (integer, higher fires first). Same priority = document order. Simultaneous firing is normal, not an error. *(Doc 5)*
- [x] **Async primitives?** The engine tick is async (tokio task). Action sequences execute sequentially, each primitive `await`ed. `spawn-agent` returns immediately (starts a tokio task, returns session ID). The agent runs asynchronously; completion fires an event that triggers a different strategy. *(Doc 5)*
- [x] **Strategy intermediate state?** Strategy-scoped `HashMap<String, serde_json::Value>` created when a strategy fires, dropped when it completes. Primitives read/write via `$context.{step-name}.{output}` references. *(Docs 2, 5)*
- [x] **Carry-over module compatibility?** Strip `#[derive(Fsm)]` from domain enums. Implement `FsmStatus` trait for kebab-case mapping. Runtime interpreter is the authority. Derive macro removed after interpreter passes v3's 128-test FSM suite. *(Doc 3)*

## Design Principles

1. **v3 behavior is the first strategy.** The YAML schema must express everything v3 does today. If it can't, the schema is incomplete - not the feature set.

2. **Sane defaults = v3 defaults.** Zero-config behavior is identical to v3. Every YAML field has a default that reproduces the v3 hardcoded value.

3. **Fail fast on bad YAML.** Validate all strategy definitions at startup. Cross-reference FSM definitions, primitive names, trigger conditions. Reject invalid compositions before any work starts.

4. **Compile-time safety trades for runtime flexibility.** We lose some compiler guarantees (exhaustive FSM state matching). Startup validation must close that gap.

5. **Primitives are the stability boundary.** Primitives are well-tested Rust functions that rarely change. Strategies are YAML that changes per experiment. The boundary between them is the API contract.

6. **Composition, not scripting (Greenspun's Tenth Rule defense).** If YAML can't express something, add a new primitive - don't add conditionals, loops, or variables to YAML. The pressure to add "just one more feature" to the YAML schema is real and must be actively resisted. The test: if a proposed YAML feature would make sense in a programming language, it belongs in a Rust primitive, not in the schema. on-success/on-failure is the ceiling of control flow in YAML, not the floor.

7. **Strategies are single-tick, not workflows.** A strategy fires and completes within one engine tick. Multi-step flows chain via events, not via in-strategy suspend/resume. This is the constraint that prevents the system from becoming a durable workflow engine (Temporal, Step Functions) and keeps the YAML schema simple. No intermediate state persistence, no saga rollback, no resume-after-crash. Self-healing comes from the reactive trigger model evaluating current state each tick.

8. **Event triggers need level-triggered fallbacks.** Ephemeral events can be lost on daemon crash. Every event-triggered recovery strategy must have a corresponding state-query trigger that catches the same condition by inspecting current state. Example: `session-failure` (event) is backed by "work is InProgress with no active session for > 60s" (state-query). The sweep strategies are the canonical pattern - level-triggered safety nets that catch whatever the event path missed.

9. **Primitives document their idempotency.** Because strategies can partially execute before a crash, primitives should be safe to re-encounter on the next tick. Read/check primitives are naturally idempotent. Mutating primitives should document whether re-execution is safe or requires guard conditions. Action sequences should order safe-to-repeat steps before hard-to-repeat steps.

10. **YAML is the single source of truth for prompts.** If a fact, constraint, or parameter is defined in YAML (FSM transitions, tool lists, scoring weights, retry limits, decomposition guidance, role capabilities), the context builder injects it into agent prompts at runtime. Prompt .pmt files contain only genuinely static prose (instructions, persona, tone). Every dynamic fact is generated from YAML - no hand-written prompt text that can drift out of sync with the actual config. Examples: valid transitions from FSM YAML, available tools from role YAML, count guidance from pipeline YAML, threshold values from trigger YAML.

    **Prompt field convention:** YAML `prompt` fields are plain strings. If the value resolves to an existing .pmt file (relative to the prompts directory), load the file. Otherwise, treat the value as inline content.

    ```yaml
    # File reference (implementer.pmt exists in prompts/)
    prompt: implementer.pmt

    # Inline content (no matching file)
    prompt: |
      Classify this plan as "brief" or "full". Respond with exactly one word.
    ```

    Simple, ergonomic, no verbose map syntax. A typo in a filename fails at startup ("file not found") rather than silently becoming inline content, because .pmt filenames are validated against the prompts directory during startup validation.

    **Alternative considered: explicit map form** (`prompt: { file: X }` vs `prompt: { inline: Y }`). More explicit but verbose and annoying to type for something that appears in every role definition and pipeline stage. The theoretical edge cases (inline text that happens to match a filename) are absurd in practice.

    **Alternative considered: templating engine (minijinja/tera) in .pmt files.** Rejected because it reintroduces coupling between static prose and dynamic facts. If a .pmt contains `{% if 'search_code' in tools %}`, logic has moved back into the prompt file. The v4 approach is that the context builder handles ALL injection - .pmt files never need conditionals because they never reference dynamic facts directly.

11. **Proven pattern (Otto precedent).** Otto proves that YAML-declares/Rust-interprets works for build orchestration. v4 applies the same pattern to agent orchestration. This is not a novel architecture - it's a domain transfer of a working system.

## References

### v4 Design Docs

- [Primitive Vocabulary](design/2026-04-11-primitive-vocabulary.md) - 58 primitives across 14 domains (Doc 2)
- [FSM-in-YAML](design/2026-04-11-fsm-in-yaml.md) - YAML schema, runtime interpreter, migration path (Doc 3)
- [Trigger and Guard System](design/2026-04-11-trigger-guard-system.md) - 27 triggers, 5 types, observation API (Doc 4)
- [Strategy Composition](design/2026-04-11-strategy-composition.md) - composition engine, v3 default strategies (Doc 5)
- [Decomposer as Strategy](design/2026-04-11-decomposer-as-strategy.md) - pipeline YAML, new decomposition strategies (Doc 6)
- [AR Trial Integration](design/2026-04-11-ar-trial-integration.md) - Python trial runner, trial configs, scoring (Doc 7)

### Pre-Vision Reference Documents

- [v4 Architecture Sketch](v4-architecture-sketch.md) - pre-design-doc thinking, skeleton, keep-vs-rewrite
- [Hardcoded Knobs Inventory](hardcoded-knobs-inventory.md) - every hardcoded parameter in v3 (motivating inventory)
- [v3 Comprehensive Evaluation](v3-comprehensive-evaluation.md) - architecture evaluation, FSM analysis, test coverage assessment
- [Loopr Architectural Analysis](2026-03-29-loopr-architectural-analysis.md) - codebase investigation and architectural evaluation
- [v2 Proven Patterns](v2-proven-patterns.md) - patterns to carry forward from v2 into v3/v4
- [Light Loops, Heavy Tools](v2-light-loops-heavy-tools.md) - v2 design principle: keep orchestration light, tools do the work
- [Domain Hierarchy](hierarchy.md) - Plan/Spec/Phase/Work decomposition hierarchy
- [Process vs Async Task](architecture-process-vs-async-task.md) - why everything is a tokio task, not OS processes
- [Seed Manifest and Tier Gate](seed-manifest-tier-gate.md) - bulk hierarchy insertion and brief/full classification
- [Chat Tunnel vs E2E Insertion](2026-04-04-chat-tunnel-vs-e2e-insertion-for-entering-a-plan.md) - how a plan enters the system
- [Related Works](related-works.md) - prior art and references (coding agents, orchestration patterns)
- [Minions: Stripe's Coding Agents](minions-stripes-one-shot-end-to-end-coding-agents.md) - industry reference for unattended coding agents

### v3 Design Docs (carry-over context)

- [Orchestration Spine](design/2026-02-25-orchestration-spine.md) - v3 orchestration architecture
- [Reactive Execution Model](design/2026-04-09-reactive-execution-model.md) - v3 reactive FSM model
- [AR Config Surface](design/2026-04-10-ar-config-surface.md) - v3 AR config surface (the "knob values" layer)

### v3 Conversations (historical context)

- [v3 Preplan Conversation](v3-preplan-conversation.md) - foundational v3 architecture decisions
- [Claude MVP and FSM Conversation](v3-claude-loopr-mvp-and-fsm-conversation.md) - MVP scoping and FSM design
- [ChatGPT Architecture Conversation](v3-chatgpt-loopr-architecture-conversation.md) - early architecture discussion

### External

- `~/repos/scottidler/otto` - otto-rs/otto, the task runner that inspired the YAML-composition pattern
- `~/repos/scottidler/keyby-rs` - keyby derive macro for YAML keyed-map deserialization (v0.1.0)
- [karpathy/autoresearch](https://github.com/karpathy/autoresearch) - autonomous ML experiment loop that inspired the AR pattern
