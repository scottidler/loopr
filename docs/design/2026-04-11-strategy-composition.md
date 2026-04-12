# Design Document: v4 Strategy Composition

**Author:** Scott A. Idler
**Date:** 2026-04-11
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

This document defines the composition engine that wires triggers to primitives, replacing v3's coordinator loop, integrator cycle, and supervisor. Strategies are YAML-defined sequences of primitives that fire when triggers match. The engine's tick loop evaluates triggers, fires matching strategies by priority, executes their action sequences, and processes success/failure wiring. v3's entire orchestration behavior is expressible as a set of default strategies.

## Problem Statement

### Background

Docs 2-4 defined the building blocks: 58 primitives (Doc 2), 5 FSMs with a runtime interpreter (Doc 3), and 27 triggers across 5 types (Doc 4). What's missing is the glue - the engine that connects "when X happens" (triggers) to "do Y" (primitives) with "then Z" (success/failure wiring).

v3 has three concurrent control loops:
- **Coordinator** - FSM-driven tick loop: sweeps, reconciliation, LLM call, action parsing, action execution
- **Integrator** - deterministic cycle: find accepted bundles, merge, validate, publish or reject
- **Supervisor** - event-driven: watch for coordinator failures, restart with exponential backoff

These loops encode orchestration policy as procedural Rust. The composition engine replaces all three with strategy-driven behavior.

### Problem

The three control loops are tightly coupled to their procedural implementations. Adding a new behavior (e.g., "pause all work when validation fails 3 times in a row") requires understanding which loop to modify, where in its control flow to insert the check, and how to wire it to the rest of the system. The composition engine must make this a YAML change.

### Goals

- Strategy YAML schema: trigger reference, action sequence, on-success/on-failure wiring, priority, scope
- Strategy lifecycle: loaded at startup, active when scope is entered, deactivated when scope exits
- Strategy scoping: any domain collection (plan, spec, phase, work, bundle, session, tick)
- The composition engine as a single tick loop replacing coordinator + integrator + supervisor
- v3's coordinator behavior expressed as default strategies (the proof point)
- Conflict resolution via explicit priority
- Strategy intermediate state (strategy-scoped context from Doc 2)

### Non-Goals

- Turing-complete strategy logic (composition, not scripting)
- Hot-reloading strategies mid-run
- Strategies that span multiple ticks (each strategy fires and completes within one engine tick; long-running work is done by agents spawned by primitives)
- GUI for strategy editing

## Proposed Solution

### Overview

The composition engine is the heart of v4. It replaces v3's three concurrent loops with a single event-driven tick loop that:

1. Collects events from the event bus
2. Evaluates all active triggers
3. For each fired trigger, looks up the associated strategy
4. Executes strategies in priority order
5. Processes on-success/on-failure wiring
6. Persists state changes

### Strategy YAML Schema

Strategies live in `strategies/` as keyed maps. The strategy name is the YAML key:

```yaml
# strategies/recovery/default.yml
work-retry-on-failure:
  description: When an agent session fails, retry the work up to max attempts
  trigger: session-failure
  scope: work
  priority: 100
  action:
    - primitive: increment-failure-count
      params:
        work-id: $trigger.scope-id
    - primitive: check-threshold
      params:
        collection: work
        id: $trigger.scope-id
        field: session-failure-count
        max: 3
  on-success:
    - primitive: retry-work
      params:
        work-id: $trigger.scope-id
  on-failure:
    - primitive: abandon-work
      params:
        work-id: $trigger.scope-id
        reason: max-session-failures-exceeded
```

**Schema fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `description` | string | no | Human-readable purpose |
| `trigger` | string | yes | Name of a trigger defined in `strategies/triggers/` |
| `scope` | string | yes | Domain collection this strategy operates on |
| `priority` | integer | no | Higher fires first. Default 100. |
| `action` | list | yes | Sequence of primitive calls, executed in order |
| `on-success` | list | no | Primitives to execute if all actions succeed |
| `on-failure` | list | no | Primitives to execute if any action fails |
| `enabled` | bool | no | Default true. Set false to disable without deleting. |

**Action step fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | no | Step name for `$context.*` references |
| `primitive` | string | yes | Primitive name from the registry |
| `params` | map | no | Parameters passed to the primitive |

**Parameter references:**

| Syntax | Resolves to |
|--------|------------|
| `$trigger.scope-id` | The record ID that the trigger fired on |
| `$trigger.event` | The event payload (for event triggers) |
| `$trigger.event.{field}` | A specific field from the event payload |
| `$context.{step-name}.{output}` | Output from a previous named step in the same strategy |
| `$config.{path}` | A value from the strategy config (role definitions, policy values) |

### Strategy Scoping

Scope determines which records a strategy watches. When a trigger fires, it produces `scope_ids` - the specific records that matched. The engine passes these to the strategy's action sequence.

Scope accepts any domain collection name: `plan`, `spec`, `phase`, `work`, `bundle`, `session`, `tick`, `lock`. This is not a hardcoded enum - if new domain types are added, they automatically become valid scopes.

A strategy is **active** for a scope ID as long as that record exists and is non-terminal. When a record reaches a terminal state, all strategies scoped to it deactivate.

### Strategy Priority and Conflict Resolution

When multiple strategies fire on the same tick:

1. **Sort by priority** (higher first)
2. **Execute in order** - each strategy runs its full action sequence before the next begins
3. **State changes are visible** - a strategy that runs earlier can change state that affects later strategies
4. **Same priority = document order** within a file, alphabetical across files

Priority ranges (convention, not enforced):

| Range | Purpose |
|-------|---------|
| 1000+ | Safety nets (circuit breakers, emergency stops) |
| 500-999 | Reconciliation (promotion, completion, sweeps) |
| 100-499 | Normal operations (assignment, retry, escalation) |
| 1-99 | Opportunistic (logging, metrics, advisory) |

### The Composition Engine

```rust
pub struct CompositionEngine {
    /// All loaded strategy definitions.
    strategies: Vec<StrategyDefinition>,
    /// The trigger evaluator (from Doc 4).
    trigger_evaluator: TriggerEvaluator,
    /// The primitive registry (from Doc 2).
    primitives: PrimitiveRegistry,
    /// The FSM interpreter (from Doc 3).
    fsm: FsmInterpreter,
    /// Cooldown tracking per (strategy, scope_id).
    cooldowns: HashMap<(String, String), Instant>,
}

impl CompositionEngine {
    /// Load all strategies from a directory. Validates at startup.
    pub fn load(
        strategies_dir: &Path,
        primitives: PrimitiveRegistry,
        fsm: FsmInterpreter,
        trigger_evaluator: TriggerEvaluator,
    ) -> eyre::Result<Self>;

    /// Run one engine tick. Called by the daemon's event loop.
    pub async fn tick(
        &mut self,
        ctx: &mut EngineContext<'_>,
    ) -> eyre::Result<TickOutcome>;
}
```

### Engine Tick Loop

The daemon runs the engine in a loop, similar to v3's coordinator FSM loop but driving ALL orchestration:

```
loop {
    // 1. Collect events since last tick
    let events = event_bus.drain();

    // 2. Evaluate push triggers (event-driven)
    let mut fired = trigger_evaluator.evaluate_events(&events, &observation_ctx);

    // 3. Evaluate pull triggers (state-driven, per-tick)
    fired.extend(trigger_evaluator.evaluate_state(&observation_ctx));

    // 4. Deduplicate and apply cooldowns
    fired.retain(|f| !engine.is_cooled_down(f));

    // 5. Look up strategies for fired triggers
    let mut pending = engine.match_strategies(&fired);

    // 6. Sort by priority (highest first)
    pending.sort_by(|a, b| b.priority.cmp(&a.priority));

    // 7. Execute each strategy
    for (strategy, trigger_result) in pending {
        let mut strategy_ctx = HashMap::new();
        strategy_ctx.insert("trigger".into(), trigger_result.to_value());

        let result = engine.execute_strategy(
            &strategy,
            &mut PrimitiveContext {
                stores, bridge, event_tx, repo_path, worktree_mgr,
                strategy_ctx: &mut strategy_ctx,
            },
        ).await;

        match result {
            Ok(_) => engine.execute_wiring(&strategy.on_success, &mut ctx).await,
            Err(_) => engine.execute_wiring(&strategy.on_failure, &mut ctx).await,
        }

        engine.record_cooldown(&strategy, &trigger_result);
    }

    // 8. Determine sleep interval
    let interval = if fired.is_empty() {
        idle_interval
    } else {
        active_interval
    };

    // 9. Sleep with event-driven wakeup
    select! {
        _ = sleep(interval) => {},
        _ = event_bus.notified() => {},
        _ = shutdown.recv() => break,
    }
}
```

### v3 Behavior as Default Strategies

This is the proof point. Every v3 behavior must be expressible as a strategy. Here are the key ones:

#### Reconciliation Strategies (v3's fixed-point loop)

```yaml
# strategies/reconciliation/promote-specs.yml
promote-pending-specs:
  description: Promote Pending specs to Active when deps are terminal and parent is active
  trigger: parent-active              # state-query trigger, re-evaluated per tick
  scope: spec
  priority: 900
  action:
    - primitive: promote-record
      params:
        collection: spec
        id: $trigger.scope-id

promote-pending-phases:
  description: Promote Pending phases to Active when deps are terminal and parent is active
  trigger: phase-promotable           # composite: parent-active AND hierarchy-deps-terminal
  scope: phase
  priority: 890
  action:
    - primitive: promote-record
      params:
        collection: phase
        id: $trigger.scope-id

promote-pending-works:
  description: Promote Pending works to Ready when deps are done and parent is active
  trigger: work-promotable            # composite: parent-active AND work-deps-done
  scope: work
  priority: 880
  action:
    - primitive: promote-record
      params:
        collection: work
        id: $trigger.scope-id

complete-phases:
  description: Complete phases when all child works are terminal
  trigger: phase-children-terminal
  scope: phase
  priority: 870
  action:
    - primitive: complete-record
      params:
        collection: phase
        id: $trigger.scope-id

complete-specs:
  description: Complete specs when all child phases are terminal
  trigger: spec-children-terminal
  scope: spec
  priority: 860
  action:
    - primitive: complete-record
      params:
        collection: spec
        id: $trigger.scope-id
```

The fixed-point convergence that v3 achieves with a loop is achieved by running reconciliation strategies in an inner loop within a single tick until no triggers fire (run-until-stable). This matches v3's behavior exactly: promoting a spec makes its children eligible, which fires in the next inner-loop iteration. The inner loop is bounded at 10 iterations, each <1ms. Total reconciliation converges in <50ms, same as v3's 3-4 loop iterations but within a single tick - no inconsistency window.

#### Sweep Strategies (v3's deterministic sweeps)

```yaml
# strategies/sweeps/default.yml
sweep-integrated-to-done:
  description: Advance all Integrated works to Done
  trigger: tick-published             # event trigger
  scope: plan
  priority: 950
  action:
    - primitive: sweep-to-done
      params:
        plan-id: $trigger.scope-id

sweep-stuck-inreview:
  description: Safety net for InReview works with all-terminal bundles
  trigger: tick-published
  scope: plan
  priority: 940
  action:
    - primitive: sweep-stuck-inreview
      params:
        plan-id: $trigger.scope-id
```

#### Recovery Strategies (v3's retry and abandon logic)

```yaml
# strategies/recovery/default.yml
work-retry-on-failure:
  description: When session fails, retry up to max failures
  trigger: session-failure
  scope: work
  priority: 200
  action:
    - primitive: increment-failure-count
      params:
        work-id: $trigger.scope-id
    - primitive: check-threshold
      params:
        collection: work
        id: $trigger.scope-id
        field: session-failure-count
        max: 3
  on-success:
    - primitive: retry-work
      params:
        work-id: $trigger.scope-id
  on-failure:
    - primitive: abandon-work
      params:
        work-id: $trigger.scope-id
        reason: max-session-failures-exceeded

# Note: check-threshold succeeds (Ok) when the threshold is NOT exceeded,
# and fails (Err) when exceeded. So on-success = "still under limit, retry"
# and on-failure = "limit reached, abandon". This reads naturally:
# "try to check the threshold; if that succeeds, retry; if it fails, abandon."

work-attempt-hard-cap:
  description: Hard cap on total work attempts
  trigger: work-retry-exhaustion
  scope: work
  priority: 1000
  action:
    - primitive: override-work
      params:
        work-id: $trigger.scope-id
        target-status: abandoned
        reason: max-attempts-exceeded

abandon-ratio-escalation:
  description: If too many works are abandoned, surface need-help
  trigger: abandon-ratio-exceeded
  scope: plan
  priority: 1100
  action:
    - primitive: escalate
      params:
        reason: abandon-ratio-exceeded
        scope-id: $trigger.scope-id
```

#### Integration Strategies (v3's integrator cycle)

```yaml
# strategies/integration/default.yml
integrate-accepted-bundles:
  description: Integrate all accepted bundles for a plan (atomic Git+DB cycle)
  trigger: bundles-ready-for-integration  # state-query: plan has Accepted bundles
  scope: plan
  priority: 500
  action:
    - primitive: integrate-tick
      params:
        plan-id: $trigger.scope-id
  # No on-failure needed: integrate-tick handles its own rollback internally
  # (fails tick, rejects bundles, resets works, reverts git state)
```

**Note:** The integration strategy uses the atomic `integrate-tick` primitive (Doc 2), NOT a chain of naked `merge-branches` + `run-validation` + `transition-record`. The Git+DB boundary is encapsulated in Rust, not exposed to YAML - the same principle that justifies `reject-bundle` and `re-decompose` as primitives. The trigger is level-triggered (state-query: "plan has Accepted bundles") rather than event-triggered, satisfying principle 8 (level-triggered fallbacks) and avoiding the event-payload-aggregation problem that cooldown-as-batching would introduce.

#### Supervisor Strategy (v3's restart logic)

```yaml
# strategies/supervision/default.yml
# Event-driven: fast reaction to coordinator failure
restart-coordinator-on-event:
  description: Restart coordinator after failure event
  trigger: coordinator-failed         # event: agent.status-changed, match coordinator+failed
  scope: session
  priority: 1200
  action:
    - primitive: check-threshold
      params:
        collection: session
        id: coordinator
        field: restart-count
        max: 5
  on-success:
    - primitive: spawn-agent
      params:
        role: coordinator
  on-failure:
    - primitive: escalate
      params:
        reason: coordinator-max-restarts-exceeded

# Level-triggered fallback (principle 8): catches missed events on restart
restart-coordinator-on-state:
  description: Restart coordinator if no Running coordinator session exists
  trigger: no-running-coordinator     # state-query: no coordinator session with status Running
  scope: plan
  priority: 1100
  cooldown-secs: 60                   # don't spam restarts
  action:
    - primitive: check-threshold
      params:
        collection: session
        id: coordinator
        field: restart-count
        max: 5
  on-success:
    - primitive: spawn-agent
      params:
        role: coordinator
```

### Strategy Lifecycle

1. **Load** - at startup, all strategy YAML files are parsed and validated
2. **Arm** - triggers associated with strategies are registered with the trigger evaluator
3. **Evaluate** - each engine tick, triggers are evaluated; fired triggers activate their strategies
4. **Execute** - the strategy's action sequence runs; on-success or on-failure wiring follows
5. **Cooldown** - after execution, the strategy enters cooldown for its scope ID (if cooldown-secs > 0)
6. **Deactivate** - when a scope record reaches a terminal state, all strategies for that scope ID stop evaluating

### Startup Validation

| Check | What It Catches |
|-------|----------------|
| Strategy trigger reference exists | Typo in trigger name |
| Strategy primitive references exist | Typo in primitive name |
| Strategy scope is a valid collection | Typo in scope |
| Strategy priority is a positive integer | Invalid priority |
| `$context.*` references point to named steps that exist earlier in the sequence | Reference to nonexistent step |
| `$context.*` output types are compatible with the consuming primitive's input types | Type mismatch between chained primitives (via output_schema/input_schema) |
| `$trigger.*` references are valid trigger output fields | Reference to nonexistent trigger field |
| No circular strategy wiring (on-success/on-failure don't create infinite loops) | Infinite strategy chains |
| All param types match primitive `validate_params` | Wrong param type |

### Implementation Plan

#### Phase 1: Strategy Data Model and Parser

1. Create `src/engine/mod.rs` with `StrategyDefinition`, `ActionStep` structs
2. Create `src/engine/schema.rs` with YAML parsing and startup validation
3. Write all default strategy YAML files in `strategies/`
4. Unit tests: parsing valid/invalid YAML, all validation checks

#### Phase 2: Composition Engine Core

1. Create `src/engine/tick.rs` with the engine tick loop
2. Implement trigger-to-strategy matching
3. Implement action sequence execution with `$context` and `$trigger` resolution
4. Implement on-success/on-failure wiring
5. Implement priority ordering and cooldown tracking
6. Integration tests: feed triggers and verify correct primitives fire in order

#### Phase 3: v3 Default Strategies

1. Write all default strategies (reconciliation, sweeps, recovery, integration, supervision)
2. Integration tests: run the same E2E scenarios as v3, verify identical outcomes
3. Verify the engine can express every v3 behavior

#### Phase 4: Daemon Integration

1. Replace coordinator, integrator, and supervisor with the composition engine
2. The daemon's main loop drives the engine tick
3. Background workers become strategies triggered by events
4. Integration tests: full daemon startup with strategy-driven orchestration

## Alternatives Considered

### Alternative 1: Keep coordinator + integrator as separate engines, strategies on top

- **Description:** Don't replace the existing loops. Add a strategy layer that can inject behavior into the existing coordinator and integrator.
- **Pros:** Lower risk. Existing loops continue to work. Strategies are additive.
- **Cons:** Two systems coexist - strategies AND procedural logic. Conflict resolution between them is undefined. Doesn't achieve the v4 goal of YAML-as-the-only-orchestration-surface.
- **Why not chosen:** The whole point is one system, not two. Strategies must be the only orchestration mechanism.

### Alternative 2: Strategies as coroutines (multi-tick execution)

- **Description:** Allow a strategy to span multiple ticks - pause after spawning an agent, resume when the agent completes.
- **Pros:** More natural for long-running flows (decompose, wait for children, then evaluate coverage).
- **Cons:** Coroutine state must survive across ticks (serialization, crash recovery). Dramatically increases complexity. Most "multi-tick" behaviors are actually multiple strategies chained by events.
- **Why not chosen:** Strategies fire and complete within one tick. Long-running work is done by agents (spawned by primitives). Multi-step flows are expressed as strategy chains: strategy A fires on event X, spawns agent; strategy B fires on agent completion event, does the next step. This is simpler and crash-resilient.

### Alternative 3: Strategy pipelines (DAG of strategies)

- **Description:** Define strategies as a DAG with dependencies, similar to how CI pipelines work.
- **Pros:** Complex flows are visually clear. Dependency resolution is automatic.
- **Cons:** Overkill. Most strategies are independent (fire on different triggers). The few that chain do so via events, which is simpler than a DAG engine. A DAG engine is another system to design, implement, and debug.
- **Why not chosen:** Event-driven chaining (strategy A emits event, strategy B triggers on it) achieves the same result with less machinery. KISS.

## Technical Considerations

### Dependencies

- **Internal:** PrimitiveRegistry (Doc 2), FsmInterpreter (Doc 3), TriggerEvaluator (Doc 4), Stores, DaemonEvent bus
- **External:** `serde_yaml` (parsing), `tokio` (async tick loop), `eyre` (error handling)
- **keyby** crate for keyed-map deserialization of strategy definitions

### Performance

- Strategy matching is O(S * T) where S = strategies and T = fired triggers per tick. With ~30 strategies and ~5 fired triggers, this is ~150 comparisons per tick - negligible.
- Action sequence execution is dominated by primitive execution time (git operations, LLM calls), not engine overhead.
- The engine tick rate is configurable: active interval (5s default) vs idle interval (30s default), matching v3.

### Security

- Strategies can only invoke registered primitives. No arbitrary code execution.
- The `$trigger` and `$context` reference system is a simple string substitution, not an expression evaluator.
- Strategy YAML files are loaded from a known directory.

### Testing Strategy

- **Parser tests:** Valid/invalid strategy YAML, all validation checks.
- **Engine tick tests:** Mock triggers, verify strategies fire in priority order, verify action sequences execute correctly.
- **Wiring tests:** on-success and on-failure chains execute correctly, context passes between steps.
- **v3 regression tests:** Express v3 default behavior as strategies, run same E2E scenarios, verify identical outcomes. This is the ultimate validation.
- **Conflict resolution tests:** Two strategies on the same trigger, verify priority ordering.

### Rollout Plan

- Implement on v4 branch, phases 1-4
- Phase 1-2 can proceed in parallel with Doc 3 (FSM) implementation
- Phase 3 (default strategies) is the proof point - block on this before declaring the engine ready
- Phase 4 (daemon integration) is the final step that replaces v3's loops

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| v3 behavior can't be expressed as strategies | Low | Critical | The default strategies section above IS the proof. If any v3 behavior can't be expressed, we add a primitive or trigger type - not scripting. |
| Per-tick convergence is slower than v3's fixed-point loop | Medium | Low | v3 converges in 3-4 loop iterations within one tick. The engine converges in 3-4 ticks. With a 5-second active interval, convergence takes 15-20 seconds vs v3's instant. Acceptable for the flexibility gained. If not, add a "run-until-stable" mode that re-evaluates within a single tick. |
| Strategy priority tuning is trial-and-error | Medium | Low | Default priority ranges are documented. v3-equivalent strategies ship with tested priorities. AR can sweep priorities as a parameter. |
| `$context` / `$trigger` reference resolution is fragile | Medium | Medium | Startup validation catches invalid references. Runtime errors include the full reference path in the error message. |
| Integration strategy is too coarse (the whole integrator cycle as one strategy) | Medium | Medium | The integration strategy shown above has 4 steps. If finer control is needed, split into multiple strategies chained by events. Start coarse, split when needed. |

## Resolved Questions

- [x] **Strategy scoping options?** Any domain collection name (plan, spec, phase, work, bundle, session, tick, lock). Not a hardcoded enum.
- [x] **Priority or document order?** Explicit priority field (integer). Higher fires first. Same priority = document order.
- [x] **Multi-tick strategies?** No. Strategies fire and complete within one tick. Long-running work is done by agents. Multi-step flows chain via events.

## Open Questions

(None remaining - all resolved during Architect review.)

## Additional Resolved Questions

- [x] **Run-until-stable for reconciliation?** Yes. Reconciliation strategies (promote-*, complete-*) run in an inner loop within a single tick until no triggers fire, matching v3's fixed-point convergence. This prevents the 15-20s inconsistency window that multi-tick convergence would create. The inner loop is bounded: maximum 10 iterations, each <1ms per strategy. Total reconciliation: <50ms per tick.
- [x] **Partial execution handling?** No rollback. Primitives are ordered safe-to-repeat first, mutating last (principle 9). `on-failure` handles cleanup. Cross-system invariants (Git+DB) are encapsulated in atomic primitives (`integrate-tick`, `reject-bundle`, `re-decompose`). Strategies that only call single-system primitives don't need rollback because each primitive either succeeds atomically or returns an error without side effects.
- [x] **Integration strategy uses integrate-tick?** Yes. The naked primitive chain was a design error caught in review. The integration strategy now uses the atomic `integrate-tick` primitive with a level-triggered fallback (principle 8).
- [x] **Level-triggered fallbacks for supervisor?** Yes. `restart-coordinator-on-event` (fast) paired with `restart-coordinator-on-state` (reliable). Both check the restart threshold.

## References

- `docs/v4-vision.md` - v4 architecture vision
- `docs/design/2026-04-11-primitive-vocabulary.md` - primitive catalog (Doc 2)
- `docs/design/2026-04-11-fsm-in-yaml.md` - FSM definitions (Doc 3)
- `docs/design/2026-04-11-trigger-guard-system.md` - trigger and guard system (Doc 4)
- `docs/hardcoded-knobs-inventory.md` - hardcoded parameters in v3
- `src/agents/coordinator/run.rs` - v3 coordinator tick loop and FSM loop
- `src/agents/integrator.rs` - v3 integrator cycle
- `src/daemon.rs` - v3 daemon main loop
- `src/daemon/supervisor.rs` - v3 supervisor restart logic
