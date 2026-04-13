# Design Document: v4 Trigger and Guard System

**Author:** Scott A. Idler
**Date:** 2026-04-11
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

This document defines the trigger evaluation framework and guard system for v4's composition engine. Triggers observe runtime state and fire when conditions are met, activating strategies. Guards are synchronous conditions on FSM transitions that must pass before the transition is allowed. Together they replace v3's hardcoded conditional logic scattered across coordinator, executor, integrator, and supervisor.

## Problem Statement

### Background

The v3 audit identified 28 distinct trigger patterns across 6 categories: threshold checks (6), ratio checks (1), event-driven reactions (5), timer/timeouts (2), state queries (9), and guards (5). These conditions are embedded in procedural Rust - `if` statements in handler functions, match arms in event loops, boolean checks in reconciliation passes. Adding a new condition requires finding the right code location, understanding the surrounding control flow, and wiring the new check into the existing logic.

### Problem

Conditions are invisible to strategy authors. You can't see what conditions exist by reading YAML - you have to read Rust. AR can tune the thresholds (e.g., change max_attempts from 3 to 5) but can't add new conditions (e.g., "if the same file is touched by 3 failed works, escalate"). The composition engine (Doc 5) needs a formal trigger system to wire conditions to actions.

### Goals

- Trigger types: threshold, ratio, event, timer, composite (and/or/not)
- Triggers observe runtime state through a read-only observation API
- Guard evaluation: synchronous, fast, no LLM calls
- Trigger lifecycle: armed, fired, cooldown (prevent trigger storms)
- All 28 v3 conditions expressible as YAML trigger definitions
- Startup validation: trigger references resolve to valid fields/events/primitives
- Triggers wire to strategies via the composition engine (Doc 5)

### Non-Goals

- Strategy composition (Doc 5 - this doc defines triggers; Doc 5 wires them to actions)
- Trigger implementation in Rust (this doc defines the schema and evaluation model)
- Complex event processing (CEP) or stream processing - triggers evaluate point-in-time state
- Triggers that call LLMs (triggers must be fast; LLM calls are primitives, not triggers)

## Proposed Solution

### Overview

The trigger system has four components:

1. **Trigger definitions** (YAML) - declare conditions with type, scope, and parameters
2. **Observation API** (Rust) - read-only interface for triggers to inspect runtime state
3. **Trigger evaluator** (Rust) - evaluates triggers per-tick (pull) or per-event (push)
4. **Guard definitions** (YAML) - conditions on FSM transitions, evaluated synchronously

### Trigger Types

#### Threshold Trigger

Fires when a numeric field on a record exceeds a limit.

Trigger definitions live in `strategies/triggers/*.yml` as keyed maps (the trigger name is the YAML key, not a field):

```yaml
# strategies/triggers/work-safety-nets.yml
work-retry-exhaustion:
  type: threshold
  scope: work
  field: attempt-count
  operator: ">="
  value: 3                          # v3's MAX_WORK_ATTEMPTS
```

**Operators:** `>=`, `>`, `<=`, `<`, `==`, `!=`

**Scope:** The record type this trigger watches. One of: `work`, `bundle`, `plan`, `spec`, `phase`, `session`, `tick`, `lock`.

**Field:** A numeric field on the scoped record. Validated at startup against the record's schema.

#### Ratio Trigger

Fires when a computed ratio exceeds a threshold.

```yaml
# strategies/triggers/plan-quality-gates.yml
abandon-ratio-exceeded:
  type: ratio
  scope: plan
numerator:
  collection: work
  filter: { status: abandoned, terminal: true }
denominator:
  collection: work
  filter: { terminal: true }
operator: ">"
value: 0.4                        # v3's max_abandon_ratio
```

**Numerator/denominator:** Each is a count query with collection and filter. The evaluator computes `count(numerator) / count(denominator)` and compares against the threshold.

#### Event Trigger

Fires when a matching event is emitted.

```yaml
# strategies/triggers/agent-events.yml
session-failure:
  type: event
  event: agent.status-changed
match:
  status: failed
scope: work                       # scoped to the work the session was working on
```

**Event types available:** `transition.completed`, `record.created`, `record.updated`, `agent.status-changed`, `agent.created`, `tick.published`, `decomposition.completed`, `decomposition.failed`, `escalation`, `reconciliation-failed`

**Match:** Optional field-value filters on the event payload. Only fires if all match conditions are satisfied.

#### Timer Trigger

Fires when elapsed time exceeds a duration.

```yaml
# strategies/triggers/work-sla.yml
work-sla-breach:
  type: timer
  scope: work
start-field: first-assignment-at   # when the clock starts
max-duration-secs: 1800            # 30 minutes
```

**start-field:** The timestamp field on the scoped record that marks when timing begins.

**max-duration-secs:** The threshold in seconds. The evaluator computes `now() - record[start-field]` and fires when it exceeds the duration.

#### State Query Trigger

Fires when a boolean state condition is true. These replace v3's reconciliation helpers.

```yaml
# strategies/triggers/reconciliation.yml
phase-children-terminal:
  type: state-query
  scope: phase
query: all-children-terminal
params:
  child-collection: work
  terminal-statuses: [done, abandoned]
```

**Built-in queries** (registered by name, like primitives):

| Query | Params | What it checks |
|-------|--------|---------------|
| `all-children-terminal` | child-collection, terminal-statuses | All children of scoped parent are in a terminal status |
| `all-children-done` | child-collection | All children have status `done` (stricter than terminal) |
| `all-deps-terminal` | dep-field, terminal-statuses | All records in the dependency list are terminal |
| `all-deps-done` | dep-field | All records in the dependency list have status `done` |
| `parent-active` | (none) | Parent record has status `active` |
| `has-children` | child-collection | At least one child exists |
| `no-active-sessions` | (none) | No Running/Paused agent sessions for this scope |
| `field-equals` | field, value | A specific field on the record equals a value |
| `field-is-true` | field | A boolean field is true |

New queries can be added as Rust functions registered in a query registry (same pattern as primitives).

#### Composite Trigger

Combines other triggers with boolean logic.

```yaml
# strategies/triggers/work-sla.yml (continued)
work-sla-full-breach:
  type: composite
  operator: and
triggers:
  - work-retry-exhaustion
  - work-sla-breach
```

**Operators:** `and`, `or`, `not` (not takes exactly one trigger)

Composites can nest: an `and` can contain an `or` which contains threshold triggers. Startup validation detects cycles.

**Composite evaluation semantics:** Composites always evaluate on the pull path (per-tick), even if they contain event sub-triggers. Event sub-triggers within composites check the event buffer in `ObservationCtx.event_bus` rather than the push pipeline. This means event triggers in composites lose sub-tick reactivity but gain consistency - all sub-triggers evaluate against the same tick snapshot.

**Cross-scope restriction:** All sub-triggers in a composite must share the same scope. A composite that `and`s `scope: work` with `scope: session` is rejected at startup validation. This prevents ambiguous `scope_ids` in the `TriggerResult`.

### Guard Definitions

Guards are conditions attached to FSM transitions. They're evaluated synchronously during `validate_transition` - if the guard fails, the transition is rejected.

Guards live in the FSM YAML files (the `guards` field from Doc 3):

```yaml
# In strategies/fsm/work.yml
guards:
  validation-required:
    from: draft
    to: active
    condition: validation-passed
    on-failure: reject
    message: "validation report required before activation"

  ac-check:
    from: in-review
    to: done
    condition: all-ac-passing
    on-failure: reject
    message: "all acceptance criteria must pass"
```

**`condition`** references a named guard condition (registered like state queries). Guard conditions must be:
- Synchronous (no async, no LLM calls)
- Fast (sub-millisecond - they run on every transition attempt)
- Pure reads (no side effects)

**Built-in guard conditions:**

| Condition | What it checks |
|-----------|---------------|
| `validation-passed` | A ValidationReport with verdict=pass exists for the target record |
| `all-ac-passing` | All acceptance criteria on the record are satisfied |
| `no-active-sessions` | No agent sessions are running for this record |
| `deps-satisfied` | All dependencies are in the required terminal state |

**`on-failure`:** What happens when the guard rejects. Currently just `reject` (transition fails with error). Could add `warn` (transition succeeds but emits warning) in the future.

### Observation API

Triggers need read-only access to runtime state. The observation API is a thin wrapper around Stores:

```rust
/// Read-only view of runtime state for trigger evaluation.
pub struct ObservationCtx<'a> {
    pub stores: &'a Stores,
    pub event_bus: &'a [DaemonEvent],   // events since last tick
    pub now: i64,                        // current timestamp (millis)
}

impl<'a> ObservationCtx<'a> {
    /// Get a record by collection and ID.
    pub fn get_record(&self, collection: &str, id: &str) -> Option<serde_json::Value>;

    /// Count records matching a filter.
    pub fn count(&self, collection: &str, filter: &Filter) -> usize;

    /// Get a numeric field from a record.
    pub fn get_field_u32(&self, collection: &str, id: &str, field: &str) -> Option<u32>;

    /// Get a timestamp field from a record.
    pub fn get_field_timestamp(&self, collection: &str, id: &str, field: &str) -> Option<i64>;

    /// Check if events matching a pattern exist in the current tick's event bus.
    pub fn has_event(&self, event_type: &str, match_filter: &Filter) -> bool;

    /// Get all children of a parent.
    pub fn children(&self, parent_id: &str, child_collection: &str) -> Vec<serde_json::Value>;
}
```

This is the ONLY way triggers access state. No direct store writes, no IPC calls, no filesystem access.

### Evaluation Model

Triggers evaluate in two modes:

**Pull (per-tick):** Threshold, ratio, timer, state-query, and composite triggers evaluate on each engine tick. The engine iterates all active triggers, calls `evaluate()` on each, and collects the ones that fired.

**Push (per-event):** Event triggers evaluate when matching events arrive. The engine's event loop checks incoming events against registered event triggers and fires matches immediately.

```rust
pub enum TriggerResult {
    /// Trigger condition not met.
    Idle,
    /// Trigger fired. Contains the scoped record ID(s) that matched
    /// and optional event payload for context propagation to strategies.
    Fired {
        scope_ids: Vec<String>,
        /// Event payload (for event triggers) or computed values (for ratio triggers).
        /// Strategies access this via $trigger.event.{field}.
        payload: Option<serde_json::Value>,
    },
}

pub trait Trigger: Send + Sync {
    fn name(&self) -> &str;
    fn evaluate(&self, ctx: &ObservationCtx<'_>) -> TriggerResult;
}
```

### Cooldown and Debounce

Triggers can fire repeatedly if the condition persists across ticks. To prevent trigger storms:

```yaml
work-retry-exhaustion:
  type: threshold
  scope: work
  field: attempt-count
  operator: ">="
  value: 3
  cooldown-secs: 60               # don't re-fire for this scope_id within 60 seconds
```

**`cooldown-secs`** (optional, default 0): After firing for a given scope ID, the trigger is suppressed for this duration. The engine tracks `last_fired_at` per (trigger-name, scope-id) pair.

Event triggers can also specify `throttle-secs` to rate-limit rapid-fire events (fire the first, drop subsequent within the window):

```yaml
session-failure:
  type: event
  event: agent.status-changed
  match:
    status: failed
  scope: work
  throttle-secs: 5               # fire first event, drop duplicates within 5 seconds
```

**Note:** This is throttling (fire-and-suppress), not debouncing (wait-for-silence-then-fire). Throttling is the right semantic for recovery triggers - you want to react immediately to the first failure, not wait for silence.

### Mapping v3 Conditions to v4 Triggers

| v3 Condition | v4 Trigger Type | YAML Name |
|-------------|----------------|-----------|
| `attempt_count >= MAX_WORK_ATTEMPTS` | threshold | `work-retry-exhaustion` |
| `session_failure_count >= max_session_failures` | threshold | `session-failure-limit` |
| `consecutive_action_count >= action_threshold` | threshold | `action-repetition-loop` |
| `same_error_count >= error_threshold` | threshold | `error-repetition-loop` |
| `consecutive_parse_failures > max_parse_retries` | threshold | `parse-failure-limit` |
| `researcher_spawn_count >= max_researcher_spawns` | threshold | `researcher-spawn-limit` |
| `requeries > max_requeries` | threshold | `self-correction-limit` |
| `restart_count >= max_restarts` | threshold | `restart-limit` |
| `abandon_ratio > max_abandon_ratio` | ratio | `abandon-ratio-exceeded` |
| agent.status_changed(failed) | event | `session-failure` |
| transition.completed | event | `transition-completed` |
| record.created | event | `record-created` |
| decomposition.completed | event | `decomposition-completed` |
| decomposition.failed | event | `decomposition-failed` |
| `now - first_assignment_at > max_wall_clock` | timer | `work-sla-breach` |
| `now - goal_start > goal_timeout` | timer | `goal-timeout` |
| all children terminal (works) | state-query | `phase-children-terminal` |
| all children terminal (phases) | state-query | `spec-children-terminal` |
| all deps terminal (hierarchy) | state-query | `hierarchy-deps-terminal` |
| all deps done (works) | state-query | `work-deps-done` |
| parent is active | state-query | `parent-active` |
| plan_approved == true | state-query | `plan-approved` |
| hierarchy exists (has children) | state-query | `hierarchy-exists` |
| goal complete (brief or full) | state-query | `goal-complete` |
| coverage verdict == incomplete | state-query | `coverage-incomplete` |
| SLA breach (attempts AND time) | composite | `work-sla-full-breach` |
| validation gate (Draft->Active) | guard | `validation-required` |
| fixed-point convergence (reconciliation) | (engine-internal, not a YAML trigger) | n/a |

**28 v3 conditions -> 27 YAML triggers + 1 engine-internal** (fixed-point convergence is a property of the reconciliation strategy's loop, not a standalone trigger).

### Startup Validation

| Check | What It Catches |
|-------|----------------|
| Trigger `scope` is a valid collection name | Typo in scope |
| Trigger `field` exists on the scoped record type | Reference to nonexistent field |
| Trigger `event` is a valid event type | Typo in event name |
| Composite trigger references exist | Reference to nonexistent trigger |
| No cycles in composite trigger references | Infinite evaluation loop |
| Guard `condition` is a registered guard condition | Reference to nonexistent condition |
| Guard `from`/`to` are valid states in the FSM | Guard on nonexistent transition |
| Guard transition actually exists in the FSM definition | Guard on impossible transition |
| Timer `start-field` is a timestamp field | Wrong field type |
| Ratio numerator/denominator collections are valid | Typo in collection name |
| Composite sub-triggers all share the same scope | Cross-scope ambiguity in scope_ids |

### Implementation Plan

#### Phase 1: Data Model and YAML Parsing

1. Create `src/trigger/mod.rs` with trigger type enums, `TriggerDefinition` struct
2. Create `src/trigger/schema.rs` with YAML parsing and startup validation
3. Define trigger YAML files in `strategies/triggers/`
4. Unit tests: parsing valid/invalid YAML, all validation checks

#### Phase 2: Observation API

1. Create `ObservationCtx` in `src/trigger/observe.rs`
2. Implement read-only wrappers around Stores
3. Register built-in state queries and guard conditions
4. Unit tests: observation queries against test stores

#### Phase 3: Trigger Evaluator

1. Create `src/trigger/evaluate.rs` with the evaluation loop
2. Implement each trigger type: threshold, ratio, event, timer, state-query, composite
3. Implement cooldown/debounce tracking
4. Port v3's conditional logic as test cases: verify each trigger fires under the same conditions v3's `if` statements would
5. Integration tests: feed events and state changes, verify correct triggers fire

#### Phase 4: Guard Evaluator

1. Create `src/trigger/guard.rs`
2. Integrate guard evaluation into the FSM interpreter (from Doc 3)
3. Register built-in guard conditions
4. Unit tests: guards that pass, guards that reject, guard error messages

## Alternatives Considered

### Alternative 1: Triggers as Rust trait objects (no YAML)

- **Description:** Define triggers in Rust using a `Trigger` trait. Strategy YAML references them by name but the conditions themselves are Rust code.
- **Pros:** Type-safe. Easy to write complex conditions. No YAML expression language.
- **Cons:** New conditions require Rust code changes. AR can't compose new conditions. Same bottleneck as v3 but with better naming.
- **Why not chosen:** The whole point is making conditions YAML-definable. Threshold and ratio triggers are the most common type and are trivially expressed in YAML.

### Alternative 2: Full expression language in YAML

- **Description:** A mini-DSL for arbitrary boolean expressions: `{ field: "attempt_count", op: ">=", value: { field: "config.max_attempts" } }`.
- **Pros:** Maximum flexibility. Can express any condition.
- **Cons:** Approaches Turing-completeness (explicit non-goal). Complex expressions are hard to validate at startup. Error messages become opaque.
- **Why not chosen:** Design principle 6: "Composition, not scripting." Named trigger types with typed parameters are clearer than expression trees.

### Alternative 3: Separate trigger evaluation service

- **Description:** Run trigger evaluation in a separate process/thread with its own event loop.
- **Pros:** Isolation. Trigger evaluation can't block the main engine.
- **Cons:** Adds IPC complexity. State synchronization between trigger service and engine. Over-engineering for what is essentially "read state, compare numbers."
- **Why not chosen:** Trigger evaluation is fast (sub-millisecond). Running it inline on the engine's tick is simpler and sufficient.

## Technical Considerations

### Dependencies

- **Internal:** Stores (read-only), DaemonEvent types, FSM interpreter (for guards)
- **External:** `serde_yaml` (parsing), `serde` (deserialization), `eyre` (error handling)
- No new external dependencies.

### Performance

- Trigger evaluation is pure state inspection: HashMap lookups, numeric comparisons, count queries. Threshold and state-query triggers are O(1) lookups. Ratio triggers are O(N) where N = records in the scoped collection (typically <200 works per plan). At 5-second tick intervals and ~30 triggers, total evaluation time is well under 100ms even at scale.
- The engine evaluates all active pull triggers per tick. With ~30 triggers and a 5-second tick interval, this is negligible.
- Event triggers are checked per-event. With ~10 event triggers and ~100 events per tick, this is still negligible.
- Guards evaluate per-transition attempt. With ~5 guards total and transitions happening at most once per tick per entity, this is negligible.

### Security

- Triggers have read-only access to state (ObservationCtx). They cannot modify state, execute commands, or make network calls.
- Guard conditions are pure functions - no side effects.
- YAML trigger definitions are loaded from `strategies/triggers/`, not arbitrary paths.

### Testing Strategy

- **Per-trigger-type tests:** Each trigger type tested with: condition met (fires), condition not met (idle), edge cases (exactly at threshold, empty collections for ratio).
- **Composite tests:** And/or/not combinations, nested composites, cycle detection.
- **Cooldown tests:** Verify suppression within cooldown window, re-fire after cooldown expires.
- **Guard tests:** Guard passes (transition allowed), guard fails (transition rejected with message), guard on nonexistent transition (startup error).
- **v3 regression tests:** For each of the 28 v3 conditions, verify the equivalent YAML trigger fires under the same circumstances.

### Rollout Plan

- Implement on v4 branch, phases 1-4
- Phase 1-2 are independent of the composition engine (can start now)
- Phase 3-4 integrate with the engine (Doc 5) and FSM interpreter (Doc 3)
- Triggers are usable standalone for testing even before the composition engine wires them to strategies

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| State query registry grows large (many one-off queries) | Medium | Low | Keep built-in queries general. Project-specific queries can be added as needed - same as primitives. |
| Trigger evaluation becomes slow with many triggers | Low | Medium | Profile. Triggers are O(1) state lookups. If needed, index triggers by scope for faster evaluation. |
| Cooldown tracking leaks memory (old scope IDs never cleaned) | Medium | Low | Deterministic TTL sweep on each engine tick: remove entries where `now - last_fired_at > cooldown_secs`. Not an LRU cache (LRU evicts wrong entries and causes trigger storms). |
| Guard conditions too restrictive (block valid transitions) | Medium | Medium | Guards have `on-failure: reject` with a clear message. Strategy authors can debug from the message. Add `on-failure: warn` option for non-blocking guards. |
| Event trigger match filters need more expressiveness | Medium | Low | Start with exact-match filters. Add regex or glob matching if needed. Don't add a query language. |

## Resolved Questions

- [x] **Trigger evaluation granularity (per-event vs per-tick)?** Both. Event triggers are push (fire on matching event). All other triggers are pull (evaluate per tick). This matches v3's implicit model.
- [x] **How do triggers observe state?** Through `ObservationCtx` - a read-only wrapper around Stores. No direct store access, no IPC, no filesystem.

## Open Questions

- [x] **State queries: YAML-definable or compiled-in Rust?** Compiled-in Rust. Design principle 6 (Greenspun defense) is decisive here. YAML-definable queries would require an expression language for field comparisons, aggregations, and filters - that's a programming language, not a composition schema. New queries are added as Rust functions registered in the query registry, same as primitives. Keeps trigger evaluation debuggable and statically validatable.
- [x] **Trigger evaluator tick rate?** Engine tick. Separate tick adds concurrency complexity (race conditions on half-written state) for zero measurable benefit. Trigger evaluation is sub-millisecond. Running on the engine tick ensures triggers observe a consistent state snapshot matching the strategies they fire.
- [x] **Cooldown persistence across restarts?** Option (b): rely on level-triggered fallbacks. Principle 8 already mandates that strategies handle redundant fires gracefully (transitions return Unchanged, creates check existence). Persisting cooldowns to a sidecar file would introduce a secondary state store outside TaskStore's JSONL-as-truth model. If a strategy can't tolerate re-fire, it's violating principle 9 (idempotency) and that's the real bug to fix.

## References

- `docs/v4-vision.md` - v4 architecture vision
- `docs/design/2026-04-11-primitive-vocabulary.md` - primitive catalog (Doc 2)
- `docs/design/2026-04-11-fsm-in-yaml.md` - FSM definitions and guards placeholder (Doc 3)
- `docs/hardcoded-knobs-inventory.md` - every hardcoded parameter in v3
- `src/agents/coordinator/run.rs` - v3 coordinator (threshold checks, event wakeup, SLA)
- `src/agents/coordinator/reconcile.rs` - v3 reconciliation (state queries, fixed-point loop)
- `src/agents/coordinator.rs` - v3 coordinator (abandon ratio, goal timeout, FSM transitions)
- `src/agents/integrator.rs` - v3 integrator (stale tick, stuck tick recovery)
- `src/agents/lifeguard.rs` - v3 lifeguard (action/error repetition, parse failures)
