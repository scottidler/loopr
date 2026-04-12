# Design Document: v4 Primitive Vocabulary

**Author:** Scott A. Idler
**Date:** 2026-04-11
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

This document catalogs every atomic operation (primitive) that the v4 composition engine can perform. Primitives are the Rust stability boundary - well-tested functions registered by name in a global registry, callable from YAML strategy definitions. This is the complete vocabulary from which all orchestration behavior is composed.

## Problem Statement

### Background

The v4 vision (docs/v4-vision.md) splits Loopr into a Rust runtime layer and a YAML strategy layer. The strategy layer composes behavior from primitives - but we can't design strategies, triggers, or the composition engine until we know exactly what the building blocks are. This doc is the foundation.

v3 has ~200+ functions across coordinator, executor, handlers, decomposer, integrator, and context builder. Many are helpers, accessors, or composed multi-step operations. The challenge is identifying which operations are truly atomic (indivisible from a strategy's perspective) vs which are compositions that should be expressed as strategy sequences.

### Problem

No formal catalog of atomic operations exists. v3's operations are scattered across modules with implicit dependencies, side effects, and ordering requirements. Without a rigorous vocabulary, strategy YAML authors (human or AR) cannot know what building blocks are available or how to compose them.

### Goals

- Catalog every primitive with: name, typed inputs, typed outputs, preconditions, side effects
- Group primitives by domain (agent, work, decompose, integrate, escalate, evaluate, context)
- Define the Primitive trait and registry pattern in Rust
- Identify which v3 operations are truly atomic vs composed
- Establish naming conventions for primitive references in YAML
- Define error handling contract (every primitive returns Result)

### Non-Goals

- Implementation of primitives (that follows this doc)
- YAML schema for strategies (Doc 5)
- Trigger definitions (Doc 4)
- FSM interpreter design (Doc 3)
- Async runtime details (covered in engine design)

## Proposed Solution

### Overview

The audit of v3 identified **59 true primitives** across 14 domains. Each primitive is a single, well-defined operation with typed inputs and outputs. Composed operations (like the integrator's full tick cycle or the coordinator's reconciliation loop) are NOT primitives - they become strategy compositions in YAML.

### The Atomicity Test

An operation is a primitive if and only if:

1. **It cannot be meaningfully subdivided** from a strategy author's perspective
2. **It has a single, clear side effect** (or is a pure query)
3. **It succeeds or fails as a unit** - no partial completion
4. **A strategy might want to skip, replace, or reorder it** relative to other operations

Operations that fail this test are compositions - sequences of primitives wired together by strategies.

### Primitive Trait

```rust
/// Declares a named, typed output field that a primitive produces.
/// Used for startup validation of $context references between primitives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputField {
    pub name: String,
    pub field_type: OutputType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OutputType {
    String,
    U32,
    U64,
    F64,
    Bool,
    StringArray,
    /// Opaque JSON for complex/variable-shape outputs.
    Json,
}

/// Declares a named, typed input parameter that a primitive accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputField {
    pub name: String,
    pub field_type: OutputType,
    pub required: bool,
}

/// The result of executing a primitive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimitiveOutput {
    /// Named, typed outputs that subsequent primitives can reference.
    /// Keys must match the names declared in output_schema().
    pub values: HashMap<String, serde_json::Value>,
    /// Human-readable summary for logging/TUI.
    pub summary: String,
}

/// Context available to every primitive during execution.
pub struct PrimitiveContext<'a> {
    pub stores: &'a Stores,
    pub bridge: &'a Bridge,
    pub event_tx: &'a broadcast::Sender<DaemonEvent>,
    pub repo_path: &'a Path,
    pub worktree_mgr: &'a WorktreeManager,
    /// Strategy-scoped scratchpad for inter-primitive communication.
    pub strategy_ctx: &'a mut HashMap<String, serde_json::Value>,
}

/// Every primitive implements this trait.
///
/// Future enhancement: consider splitting into Primitive (side-effecting, takes &mut ctx)
/// and QueryPrimitive (pure read, takes &ctx). This would let the engine run multiple
/// queries concurrently within a strategy's "gather" phase, since &ctx doesn't require
/// exclusive access. Not worth the complexity yet - start with one trait, split when
/// query parallelism becomes a measurable bottleneck.
pub trait Primitive: Send + Sync {
    /// Unique name used in YAML references (e.g., "spawn-agent").
    fn name(&self) -> &'static str;

    /// Execute the primitive.
    fn execute(
        &self,
        ctx: &mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + '_>>;

    /// Declare the typed output fields this primitive produces.
    /// Used at startup to validate $context references between primitives:
    /// if step B references $context.step-A.session-id, the engine verifies
    /// that step A's primitive declares an output named "session-id" of a
    /// compatible type with step B's expected input type.
    fn output_schema(&self) -> Vec<OutputField>;

    /// Declare the typed input params this primitive accepts.
    /// Used at startup to validate strategy YAML params and $context type
    /// compatibility. Each InputField has a name, type, and required flag.
    fn input_schema(&self) -> Vec<InputField>;

    /// Validate params at startup (before any work starts).
    /// Default implementation checks params against input_schema().
    fn validate_params(&self, params: &serde_json::Value) -> eyre::Result<()>;

    /// Idempotency guarantee. Strategies can partially execute before a crash;
    /// the next tick re-evaluates triggers and may re-invoke primitives that
    /// already ran. Primitives must document their behavior on re-execution:
    /// - Idempotent: safe to call again (transitions return Unchanged, queries are pure)
    /// - GuardRequired: caller must check precondition before re-calling (create-* primitives)
    /// - NonIdempotent: re-execution produces duplicate side effects (must be last in sequence)
    fn idempotency(&self) -> Idempotency;

    /// Whether this primitive requires exclusive git worktree access.
    /// If true, the engine acquires a centralized async git mutex before
    /// calling execute(). Default: false.
    fn requires_git_lock(&self) -> bool { false }
}

#[derive(Debug, Clone, Copy)]
pub enum Idempotency {
    /// Safe to call multiple times with same params. No duplicate side effects.
    Idempotent,
    /// Safe if a guard condition is checked first (e.g., "record doesn't already exist").
    GuardRequired,
    /// NOT safe to re-call. Must be last in action sequence or protected by cooldown.
    NonIdempotent,
}
```

### Registry

```rust
pub struct PrimitiveRegistry {
    primitives: HashMap<String, Box<dyn Primitive>>,
}

impl PrimitiveRegistry {
    pub fn new() -> Self { /* ... */ }

    /// Register a primitive. Called at startup.
    pub fn register(&mut self, primitive: Box<dyn Primitive>) {
        self.primitives.insert(primitive.name().to_string(), primitive);
    }

    /// Look up a primitive by name. Returns None if not found.
    pub fn get(&self, name: &str) -> Option<&dyn Primitive> {
        self.primitives.get(name).map(|p| p.as_ref())
    }

    /// Validate that all primitive names referenced in YAML exist.
    pub fn validate_references(&self, names: &[String]) -> Vec<String> {
        names.iter()
            .filter(|n| !self.primitives.contains_key(n.as_str()))
            .cloned()
            .collect()
    }
}
```

### Naming Convention

- Primitive names are kebab-case: `spawn-agent`, `transition-work`, `merge-branches`
- Domain prefix groups related primitives: `agent-*`, `work-*`, `decompose-*`, etc.
- However, the prefix is a convention, not enforced - some primitives span domains

### Output Chaining

Primitives within a strategy chain through the strategy-scoped context (`strategy_ctx`). Each primitive writes its outputs to the context under a namespaced key; subsequent primitives read from it. YAML references outputs using `$context.{step-name}.{output-name}` syntax.

Example: the ask-a-friend recovery strategy chains `spawn-agent` output into `inject-context`:

```yaml
action:
  - name: spawn-advisor
    primitive: spawn-agent
    params:
      role: reviewer
      model: claude-opus-4-6
      target-id: $trigger.work-id
  - name: inject-advice
    primitive: inject-context
    params:
      session-id: $trigger.session-id
      content: $context.spawn-advisor.session-id
      source: advisor-session
  - primitive: retry-work
    params:
      work-id: $trigger.work-id
```

The `$context.spawn-advisor.session-id` reference resolves at runtime by reading the `spawn-advisor` step's `PrimitiveOutput.values["session-id"]` from the strategy context. The `$trigger.*` references come from the trigger that fired the strategy (defined in Doc 4).

**Startup type validation:** Every `$context` reference is validated at YAML load time against primitive schemas. When the engine encounters `$context.spawn-advisor.session-id` as an input to `inject-context`'s `session-id` param:

1. It checks that step `spawn-advisor` exists earlier in the action sequence
2. It checks that `spawn-agent`'s `output_schema()` declares a field named `session-id`
3. It checks that the output field's `OutputType` is compatible with `inject-context`'s `input_schema()` type for `session-id` (both `OutputType::String`)

If any check fails, the strategy is rejected at startup with a clear error message naming the step, field, and type mismatch. This catches wiring errors before any work starts rather than producing opaque runtime failures.

**Alternative considered: untyped JSON with runtime errors.** We considered keeping `PrimitiveOutput.values` as untyped `serde_json::Value` with no schema validation, relying on runtime error messages when a primitive receives an unexpected type. This is simpler to implement (no schema declarations on every primitive) but pushes type errors to execution time, where they surface as confusing "expected string, got null" errors mid-strategy. The typed schema approach costs ~5 lines per primitive (declaring input/output fields) but catches every wiring error at startup. Given that AR will generate novel strategy compositions overnight without human supervision, startup validation is worth the boilerplate.

## Primitive Catalog

### Domain 1: Agent Lifecycle (5 primitives)

#### `spawn-agent`

Starts a new agent session for a given role and target.

| Field | Value |
|-------|-------|
| **Params** | `role` (string: implementer, reviewer, researcher, integrator, coordinator), `target-id` (string: work or bundle ID), `model` (string, optional override), `context-from` (string, optional: session ID to inherit context from) |
| **Outputs** | `session-id` (string) |
| **Preconditions** | Target exists and is in assignable state. Pool capacity not exceeded. |
| **Side Effects** | Creates AgentSession record. Spawns tokio task. Emits `agent.created` event. For implementer: transitions Work Draft->Ready->InProgress if needed. |
| **v3 Source** | `handle_agent_start`, `handle_assign_agent` |

#### `stop-agent`

Cancels a running agent session.

| Field | Value |
|-------|-------|
| **Params** | `session-id` (string) |
| **Outputs** | (none) |
| **Preconditions** | Session exists and is Running or Paused. |
| **Side Effects** | Aborts tokio task. Transitions session to Stopped. Emits `agent.status_changed` event. |
| **v3 Source** | `handle_agent_stop` |

#### `pause-agent`

Pauses a running agent session.

| Field | Value |
|-------|-------|
| **Params** | `session-id` (string) |
| **Outputs** | (none) |
| **Preconditions** | Session exists and is Running. |
| **Side Effects** | Transitions session to Paused. Emits `agent.status_changed` event. |
| **v3 Source** | `handle_agent_pause` |

#### `resume-agent`

Resumes a paused agent session.

| Field | Value |
|-------|-------|
| **Params** | `session-id` (string) |
| **Outputs** | (none) |
| **Preconditions** | Session exists and is Paused. |
| **Side Effects** | Transitions session to Running. Emits `agent.status_changed` event. |
| **v3 Source** | `handle_agent_resume` |

#### `inject-context`

Injects additional context into a running agent's session.

| Field | Value |
|-------|-------|
| **Params** | `session-id` (string), `content` (string: context to inject), `source` (string, optional: where the context came from) |
| **Outputs** | (none) |
| **Preconditions** | Session exists and is Running. |
| **Side Effects** | Appends context to session's message history. |
| **v3 Source** | New in v4 (referenced in vision doc's ask-a-friend example). In v3, context injection was implicit through the context builder. |

### Domain 2: Work Management (7 primitives)

#### `create-work`

Creates a new Work record under a parent (Phase or Plan in brief mode).

| Field | Value |
|-------|-------|
| **Params** | `parent-id` (string), `title` (string), `files` (string[], optional), `acceptance-criteria` (string[], optional), `dependencies` (string[], optional: work IDs or `batch:N` references) |
| **Outputs** | `work-id` (string) |
| **Preconditions** | Parent exists. |
| **Side Effects** | Creates Work in Draft status. Persists to TaskStore. Emits `record.created` event. |
| **v3 Source** | `handle_work_create` |

#### `transition-work`

Normal FSM transition for a Work item. This is a convenience wrapper around `transition-record` with Work-specific validation (dependency checks, assignee management). Strategies should use this for Work transitions rather than the generic `transition-record`.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string), `target-status` (string), `role` (string) |
| **Outputs** | `from-status` (string), `to-status` (string) |
| **Preconditions** | Work exists. Transition is valid per FSM definition for given role. |
| **Side Effects** | Updates Work status. Persists to TaskStore. Emits `transition.completed` event. |
| **v3 Source** | `handle_work_transition` |

#### `override-work`

Force-transition a Work with audit trail, bypassing normal FSM guards.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string), `target-status` (string), `reason` (string) |
| **Outputs** | `from-status` (string), `to-status` (string) |
| **Preconditions** | Work exists. Override transition is valid per FSM definition. |
| **Side Effects** | Transitions Work. Stops active sessions for that Work. Releases advisory locks. Creates audit Learning. Persists to TaskStore. Emits `transition.completed` event. |
| **v3 Source** | `handle_override_work` |

#### `claim-next-work`

Priority-scored atomic claim of the next Ready work from the queue.

| Field | Value |
|-------|-------|
| **Params** | (none - uses global work queue) |
| **Outputs** | `work-id` (string, or null if none available) |
| **Preconditions** | At least one Ready work with all dependencies Done and no active implementer session. |
| **Side Effects** | Atomically transitions Work from Ready to InProgress (two-phase lock). Sets assignee. Persists to TaskStore. Priority score: `(10 - min(deps, 10)) * 10 - min(attempt_count, 5) * 50`. |
| **v3 Source** | `next_assignable_work` in work_queue.rs |

#### `increment-failure-count`

Increments a Work's session failure counter.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string) |
| **Outputs** | `count` (u32: new failure count) |
| **Preconditions** | Work exists. |
| **Side Effects** | Updates Work.session_failure_count. Persists to TaskStore. |
| **v3 Source** | Implicit in executor lifecycle.rs session failure tracking |

#### `increment-attempt-count`

Increments a Work's attempt counter.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string) |
| **Outputs** | `count` (u32: new attempt count) |
| **Preconditions** | Work exists. |
| **Side Effects** | Updates Work.attempt_count. Persists to TaskStore. |
| **v3 Source** | `CoordinatorState::increment_attempts` |

#### `reset-work`

Resets a Work to Ready after bundle rejection or failure, with reason tracking.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string), `reason` (string) |
| **Outputs** | (none) |
| **Preconditions** | Work exists. |
| **Side Effects** | Overrides Work to Ready. Creates Learning documenting the reason. Persists to TaskStore. |
| **v3 Source** | `reset_work_after_bundle_rejection` |

### Domain 3: Record CRUD (5 primitives)

These are generic record operations that apply across domain types. Domain-specific primitives like `create-work` and `transition-work` are thin wrappers around these generics that add domain validation (e.g., dependency checking, assignee management). Strategy authors should prefer domain-specific primitives when available; the generics exist for cases where no domain-specific wrapper is needed (e.g., transitioning a Tick or creating a Lock).

#### `create-record`

Creates a new record of any domain type.

| Field | Value |
|-------|-------|
| **Params** | `collection` (string: plan, spec, phase, work, bundle, tick, lock, learning), `fields` (object: type-specific fields) |
| **Outputs** | `id` (string) |
| **Preconditions** | Fields satisfy type-specific validation. |
| **Side Effects** | Creates record in Draft/initial status. Persists to TaskStore. Emits `record.created` event. |
| **v3 Source** | `handle_plan_create`, `handle_spec_create`, `handle_phase_create`, `handle_tick_create`, etc. |

#### `update-record`

Updates fields on an existing record.

| Field | Value |
|-------|-------|
| **Params** | `collection` (string), `id` (string), `fields` (object: fields to update) |
| **Outputs** | (none) |
| **Preconditions** | Record exists. |
| **Side Effects** | Updates specified fields. Sets updated_at. Persists to TaskStore. Emits `record.updated` event. |
| **v3 Source** | `handle_plan_update`, `handle_work_update`, etc. |

#### `transition-record`

Generic FSM transition for any domain type.

| Field | Value |
|-------|-------|
| **Params** | `collection` (string), `id` (string), `target-status` (string), `role` (string) |
| **Outputs** | `from-status` (string), `to-status` (string) |
| **Preconditions** | Record exists. Transition valid per FSM definition. |
| **Side Effects** | Updates status. Persists to TaskStore. Emits `transition.completed` event. May run validation gate (Draft->Active). |
| **v3 Source** | All `handle_*_transition` handlers |

#### `query-records`

Read/filter records from a collection. Pure query, no side effects.

| Field | Value |
|-------|-------|
| **Params** | `collection` (string), `filters` (object, optional: field-value pairs to match) |
| **Outputs** | `records` (array of objects) |
| **Preconditions** | None. |
| **Side Effects** | None (pure read). |
| **v3 Source** | Various store reads throughout v3 |

#### `get-record`

Read a single record by ID. Pure query, no side effects.

| Field | Value |
|-------|-------|
| **Params** | `collection` (string), `id` (string) |
| **Outputs** | `record` (object) |
| **Preconditions** | Record exists. |
| **Side Effects** | None (pure read). |
| **v3 Source** | Various store reads throughout v3 |

### Domain 4: Bundle Operations (3 primitives)

#### `create-bundle`

Creates a Bundle from an implementer's work output.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string), `branch-name` (string), `description` (string), `claims` (string[]), `head-commit` (string), `paths` (string[]), `base-tick-id` (string, optional), `is-noop` (bool), `noop-reason` (string, optional) |
| **Outputs** | `bundle-id` (string) |
| **Preconditions** | Work exists. Branch exists (unless noop). |
| **Side Effects** | Creates Bundle in Draft status. Persists to TaskStore. Emits `record.created` event. |
| **v3 Source** | `handle_bundle_create` |

#### `reject-bundle`

Rejects a bundle and cascades: resets the parent Work to Ready.

| Field | Value |
|-------|-------|
| **Params** | `bundle-id` (string), `reason` (string) |
| **Outputs** | (none) |
| **Preconditions** | Bundle exists and is in rejectable state. |
| **Side Effects** | Transitions Bundle to Rejected. Calls `reset-work` for parent Work. Emits `transition.completed` event. |
| **v3 Source** | Stale bundle rejection in integrator `run_cycle` |

**Note:** `reject-bundle` is a borderline composition (transition + reset-work). It's included as a primitive because bundle rejection is always paired with work reset - splitting them would be an error-prone footgun. Strategy authors should never need to call transition-record(bundle, Rejected) + reset-work separately.

#### `supersede-bundles`

Marks all non-terminal bundles for a work as Superseded when a new bundle is created.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string), `except-bundle-id` (string: the new bundle to keep) |
| **Outputs** | `count` (u32: number superseded) |
| **Preconditions** | Work exists. |
| **Side Effects** | Transitions matching bundles to Superseded. Persists to TaskStore. |
| **v3 Source** | Implicit in bundle lifecycle (new bundle supersedes old ones) |

### Domain 5: Decomposition (7 primitives)

#### `decompose`

Breaks a parent document into children via LLM call.

| Field | Value |
|-------|-------|
| **Params** | `parent-id` (string), `parent-collection` (string: plan, spec, phase), `target-kind` (string: spec, phase, work), `prompt` (string, optional: .pmt file path or inline content per vision principle 10) |
| **Outputs** | `children` (array: [{id, title, kind, dependencies, acceptance-criteria}]) |
| **Preconditions** | Parent exists and has content. |
| **Side Effects** | Creates child records in Pending status. Persists to TaskStore. Emits `decomposition.completed` or `decomposition.failed` event. |
| **v3 Source** | `decompose_into`, `decompose_hierarchy` |

#### `validate-document`

Validates a document against its type's schema/template via LLM.

| Field | Value |
|-------|-------|
| **Params** | `collection` (string), `id` (string) |
| **Outputs** | `verdict` (string: pass, fail, warn), `issues` (array), `summary` (string) |
| **Preconditions** | Record exists and has content. |
| **Side Effects** | Creates ValidationReport record. Persists to TaskStore. |
| **v3 Source** | `DocValidator::validate_plan/spec/phase` |

#### `evaluate-coverage`

Checks whether children adequately cover parent requirements via LLM.

| Field | Value |
|-------|-------|
| **Params** | `parent-collection` (string), `parent-id` (string) |
| **Outputs** | `verdict` (string: complete, incomplete), `gaps` (array), `summary` (string) |
| **Preconditions** | Parent exists. At least one child exists. |
| **Side Effects** | Creates CoverageReport record. Persists to TaskStore. |
| **v3 Source** | `CoverageEvaluator::evaluate_plan_specs/spec_phases/phase_works` |

#### `ratify-hierarchy`

Bottom-up semantic validation of parent-children relationships via LLM.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string) |
| **Outputs** | `passed` (bool), `issues` (array) |
| **Preconditions** | Plan exists with decomposed children. |
| **Side Effects** | Logs warnings for issues (non-blocking). |
| **v3 Source** | `ratify_hierarchy` |

#### `abandon-children`

Abandons all non-terminal children of a parent, preserving specified children that are still valid.

| Field | Value |
|-------|-------|
| **Params** | `parent-id` (string), `parent-collection` (string: plan, spec, phase), `preserve-ids` (string[], optional: children to keep) |
| **Outputs** | `abandoned-count` (u32), `preserved-count` (u32) |
| **Preconditions** | Parent exists. |
| **Side Effects** | Transitions all non-terminal, non-preserved children to Abandoned. Creates Learning documenting the abandonment. Persists to TaskStore. |
| **v3 Source** | Partial: v3's `ReviseParent` transitions parent back to Draft, but doesn't selectively preserve children. This primitive is more surgical. |

#### `re-decompose`

Re-decomposes a parent after new knowledge invalidates the existing decomposition. Abandons existing children (except preserved ones), injects the reason as context, and re-runs decomposition.

| Field | Value |
|-------|-------|
| **Params** | `parent-id` (string), `parent-collection` (string: plan, spec, phase), `target-kind` (string: spec, phase, work), `reason` (string: why re-decomposition is needed), `preserve-ids` (string[], optional: children to keep), `prompt` (string, optional: .pmt file path or inline content per vision principle 10) |
| **Outputs** | `children` (array: [{id, title, kind, dependencies, acceptance-criteria}]), `abandoned-count` (u32) |
| **Preconditions** | Parent exists and has content. |
| **Side Effects** | Calls `abandon-children` for non-preserved children. Re-runs decomposition with reason injected into context (so the LLM knows what went wrong). Creates child records in Pending status. Emits `decomposition.completed` event. |
| **v3 Source** | v3's `ReviseParent` + re-decompose flow, but formalized as a single primitive. Encapsulates the invariant that you never re-decompose without first dealing with existing children. |

**Note:** `re-decompose` is a borderline composition (abandon-children + decompose). Like `reject-bundle`, it encapsulates a critical invariant: re-decomposition without child cleanup creates orphaned records. Strategy authors should use `re-decompose` when new knowledge arrives; use bare `decompose` only for first-time decomposition of a fresh parent.

#### `classify-tier`

Binary classifier: determines brief vs full decomposition path.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string) |
| **Outputs** | `tier` (string: brief or full) |
| **Preconditions** | Plan exists with content. |
| **Side Effects** | None (pure classification). |
| **v3 Source** | Tier-gate classification in coordinator |

### Domain 6: Integration (7 primitives)

#### `integrate-tick`

Executes the full integration cycle atomically: create tick, merge bundle branches, run validation, publish or fail. Encapsulates the Git+DB boundary - the same reasoning that makes `reject-bundle` a primitive (DB invariant protection) applies here for the more fragile Git+DB synchronization. If the daemon crashes mid-integration, the git audit primitives detect and repair the fracture on next startup.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string), `bundle-ids` (string[]) |
| **Outputs** | `tick-id` (string), `outcome` (string: published, validation-failed, merge-conflict), `head-sha` (string, if published) |
| **Preconditions** | No in-progress Tick exists. All bundles are Accepted. Integration branch exists. |
| **Side Effects** | Creates Tick. Transitions bundles to Integrating. Merges branches. Runs validation. On success: publishes Tick, transitions bundles to Merged, transitions works to Integrated. On failure: fails Tick, rejects bundles, resets works. All Git+DB mutations are internally sequenced with rollback on failure. |
| **Idempotency** | GuardRequired - check no in-progress tick before calling. |
| **v3 Source** | `run_cycle` in integrator.rs - the entire integration cycle. |

**Note:** The individual integration primitives (`create-tick`, `merge-branches`, `run-validation`) remain available for strategies that need finer control. But the default integration strategy should use `integrate-tick` to avoid the Git+DB split-brain risk that the Architect identified. Like `reject-bundle` and `re-decompose`, this encapsulates a cross-system invariant in Rust rather than trusting YAML authors to sequence it correctly.

#### `create-tick`

Creates a new Tick for bundling accepted work.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string), `number` (u32, optional: auto-incremented if omitted) |
| **Outputs** | `tick-id` (string) |
| **Preconditions** | No in-progress Tick exists. |
| **Side Effects** | Creates Tick in Open status. Persists to TaskStore. Emits `record.created` event. |
| **v3 Source** | `handle_tick_create` |

#### `merge-branches`

**Git concurrency enforcement:** All git-mutating primitives (`merge-branches`, `run-validation`, `create-integration-branch`, `merge-integration-to-main`, `delete-integration-branch`, `integrate-tick`) must acquire a centralized async git mutex held in `PrimitiveContext` before executing. This is a hard requirement, not a convention. Priority ordering alone is insufficient - it does not serialize git operations across different plans or protect against edge-case race conditions. The mutex is acquired automatically by the engine when it detects a git-mutating primitive (via a `requires_git_lock()` method on the trait), not manually by YAML authors.

Merges one or more bundle branches into the integration branch.

| Field | Value |
|-------|-------|
| **Params** | `branch-names` (string[]), `target-branch` (string: integration branch name) |
| **Outputs** | `head-sha` (string: final HEAD after merges), `conflicts` (array, empty if clean) |
| **Preconditions** | Target branch exists. All source branches exist. |
| **Side Effects** | Git merge operations. On conflict: `git merge --abort`, returns error with conflict info. |
| **v3 Source** | `merge_bundle_branches` |

#### `run-validation`

Executes validation commands (test, lint, format) against the current state.

| Field | Value |
|-------|-------|
| **Params** | `commands` (string[]), `timeout-secs` (u32, optional: per-command timeout) |
| **Outputs** | `passed` (bool), `log` (string: command output) |
| **Preconditions** | Working directory is clean (post-merge). |
| **Side Effects** | Executes shell commands. Captures stdout/stderr. |
| **v3 Source** | `run_validation_commands` |

#### `create-integration-branch`

Creates a per-plan integration branch from main.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string) |
| **Outputs** | `branch-name` (string: "integration/{plan-id}") |
| **Preconditions** | Branch does not already exist. |
| **Side Effects** | Creates git branch. |
| **v3 Source** | Integration branch creation in `apply_fsm_transition` |

#### `merge-integration-to-main`

Merges the integration branch to main and cleans up.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string), `message` (string: merge commit message) |
| **Outputs** | (none) |
| **Preconditions** | Integration branch exists. Main is clean. |
| **Side Effects** | Checkout main. Merge --no-ff. Delete integration branch. |
| **v3 Source** | `merge_integration_branch_to_main` |

#### `delete-integration-branch`

Deletes an integration branch without merging (plan failure/abandonment).

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string) |
| **Outputs** | (none) |
| **Preconditions** | Integration branch exists. |
| **Side Effects** | Checkout main. Force-delete branch. |
| **v3 Source** | `delete_integration_branch` |

### Domain 7: Worktree (4 primitives)

#### `create-worktree`

Creates an isolated git worktree for an agent.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string), `base-ref` (string: SHA or branch name) |
| **Outputs** | `worktree-path` (string) |
| **Preconditions** | Worktree does not already exist for this work-id. |
| **Side Effects** | Creates worktree directory. Creates or resets agent branch. |
| **v3 Source** | `WorktreeManager::create_branch`, `get_or_create_branch` |

#### `cleanup-worktree`

Removes a worktree from the filesystem.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string) |
| **Outputs** | (none) |
| **Preconditions** | Worktree exists. |
| **Side Effects** | `git worktree remove --force`. Does NOT delete agent branch. |
| **v3 Source** | `WorktreeManager::cleanup` |

#### `delete-agent-branch`

Deletes the agent branch after work is integrated or abandoned.

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string) |
| **Outputs** | (none) |
| **Preconditions** | None (idempotent - ignores "not found"). |
| **Side Effects** | `git branch -D agent/{work-id}`. |
| **v3 Source** | `WorktreeManager::delete_branch` |

#### `refresh-worktree`

Rebases a worktree to a new base reference (clears staleness).

| Field | Value |
|-------|-------|
| **Params** | `work-id` (string), `new-base-ref` (string) |
| **Outputs** | `success` (bool) |
| **Preconditions** | Worktree exists. |
| **Side Effects** | `git rebase`. On failure: `git rebase --abort`. |
| **v3 Source** | `WorktreeManager::refresh` |

### Domain 8: Context & Learning (4 primitives)

#### `build-context`

Assembles the full LLM context for an agent session.

| Field | Value |
|-------|-------|
| **Params** | `session-id` (string), `role` (string), `target-id` (string: work or bundle ID), `iteration` (u32), `previous-summary` (string, optional) |
| **Outputs** | `system-prompt` (string), `user-message` (string), `token-estimate` (usize) |
| **Preconditions** | Target exists. |
| **Side Effects** | Reads filesystem (docs, repo files). Pure assembly, no writes. |
| **v3 Source** | `ContextBuilder::build` |

#### `create-learning`

Records a learning (insight, failure reason, structural decision) for future context.

| Field | Value |
|-------|-------|
| **Params** | `content` (string), `scope` (string: global, plan, spec, phase, work), `source-id` (string), `applicable-roles` (string[], optional), `files` (string[], optional) |
| **Outputs** | `learning-id` (string) |
| **Preconditions** | Source record exists. |
| **Side Effects** | Creates Learning record. Persists to TaskStore. |
| **v3 Source** | `handle_learning_create` |

#### `select-learnings`

Filters and ranks learnings for a given scope and role. Pure query.

| Field | Value |
|-------|-------|
| **Params** | `scope-ids` (array of {id, scope} pairs), `role` (string), `min-confidence` (f32, optional: default 0.0), `max-count` (usize, optional: default 20) |
| **Outputs** | `learnings` (array of learning objects) |
| **Preconditions** | None. |
| **Side Effects** | None (pure read). |
| **v3 Source** | `select_learnings` |

#### `build-state-summary`

Assembles a state summary of current orchestration state for the coordinator's LLM context.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string), `include-sla` (bool, optional: annotate with SLA breach info) |
| **Outputs** | `summary` (string: markdown-formatted state summary) |
| **Preconditions** | Plan exists. |
| **Side Effects** | None (pure read + assembly). |
| **v3 Source** | `build_state_summary`, `build_state_summary_with_sla` |

#### `compact-context`

Truncates context sections to fit within token budget.

| Field | Value |
|-------|-------|
| **Params** | `text` (string), `max-tokens` (usize), `strategy` (string: head, tail, prose) |
| **Outputs** | `truncated` (string), `was-truncated` (bool) |
| **Preconditions** | None. |
| **Side Effects** | None (pure transform). |
| **v3 Source** | `truncate_prose`, `truncate_from_head`, `truncate_list` |

### Domain 9: Reconciliation & Escalation (8 primitives)

#### `promote-record`

Promotes a Pending record to Active/Ready when its dependencies are satisfied.

| Field | Value |
|-------|-------|
| **Params** | `collection` (string: spec, phase, work), `id` (string) |
| **Outputs** | `promoted` (bool) |
| **Preconditions** | Record is Pending. Parent is Active. Dependencies are terminal (specs/phases) or Done (works). |
| **Side Effects** | Transitions record to Active (specs/phases) or Ready (works). Persists to TaskStore. |
| **v3 Source** | `promote_specs`, `promote_phases`, `promote_works` |

#### `complete-record`

Marks a record as Complete when all its children are terminal.

| Field | Value |
|-------|-------|
| **Params** | `collection` (string: spec, phase), `id` (string) |
| **Outputs** | `completed` (bool) |
| **Preconditions** | Record is Active. All children are terminal (Done or Abandoned). |
| **Side Effects** | Transitions record to Complete. Persists to TaskStore. |
| **v3 Source** | `complete_phases`, `complete_specs` |

#### `detect-goal-complete`

Checks whether a Plan's goal is fully achieved. Pure query.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string) |
| **Outputs** | `complete` (bool), `done-count` (u32), `total-count` (u32), `abandoned-count` (u32) |
| **Preconditions** | Plan exists. |
| **Side Effects** | None (pure read). |
| **v3 Source** | `detect_goal_complete`, `goal_work_counts` |

#### `check-threshold`

Checks a numeric field against a maximum value. Pure query.

| Field | Value |
|-------|-------|
| **Params** | `collection` (string), `id` (string), `field` (string: session-failure-count, attempt-count, etc.), `max` (u32) |
| **Outputs** | `exceeded` (bool), `current` (u32) |
| **Preconditions** | Record exists. |
| **Side Effects** | None (pure read). |
| **v3 Source** | Various threshold checks scattered across v3 |

#### `check-ratio`

Checks a ratio (numerator/denominator) against a threshold. Pure query.

| Field | Value |
|-------|-------|
| **Params** | `numerator-query` (object: collection + status filter), `denominator-query` (object: collection + status filter), `scope-id` (string: plan ID), `threshold` (f64) |
| **Outputs** | `exceeded` (bool), `ratio` (f64) |
| **Preconditions** | Scope record exists. |
| **Side Effects** | None (pure read). |
| **v3 Source** | `check_abandon_gate`, `goal_abandon_ratio` |

#### `escalate`

Surfaces a need-help condition. The orchestration equivalent of "I'm stuck."

| Field | Value |
|-------|-------|
| **Params** | `reason` (string), `scope-id` (string, optional: plan or work ID), `details` (object, optional) |
| **Outputs** | (none) |
| **Preconditions** | None. |
| **Side Effects** | Emits `escalation` event. May transition plan to NeedHelp. Creates Learning. |
| **v3 Source** | NeedHelp return path in coordinator |

#### `sweep-to-done`

Deterministic sweep: transitions all Integrated works to Done.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string) |
| **Outputs** | `count` (u32: works transitioned) |
| **Preconditions** | Plan exists. |
| **Side Effects** | Transitions Integrated works to Done. Persists to TaskStore. |
| **v3 Source** | `sweep_integrated_to_done` |

#### `sweep-stuck-inreview`

Safety net: advances InReview works whose bundles are all terminal with at least one Merged.

| Field | Value |
|-------|-------|
| **Params** | `plan-id` (string) |
| **Outputs** | `count` (u32: works advanced) |
| **Preconditions** | Plan exists. |
| **Side Effects** | Transitions stuck InReview works to Integrated. Persists to TaskStore. |
| **v3 Source** | `sweep_stuck_inreview` |

### Domain 10: Lock Management (2 primitives)

#### `acquire-lock`

Creates an advisory lock on a resource (file path).

| Field | Value |
|-------|-------|
| **Params** | `resource` (string: file path), `holder-id` (string: work ID), `ttl-secs` (u32, optional) |
| **Outputs** | `lock-id` (string), `acquired` (bool: false if already held by another) |
| **Preconditions** | None. |
| **Side Effects** | Creates Lock record if not already held. Auto-expires stale locks. Persists to TaskStore. |
| **v3 Source** | `handle_lock_create`, `auto_acquire_write_lock` |

#### `release-lock`

Releases an advisory lock.

| Field | Value |
|-------|-------|
| **Params** | `lock-id` (string) OR `holder-id` (string: releases all locks for holder) |
| **Outputs** | `count` (u32: locks released) |
| **Preconditions** | Lock exists and is Active. |
| **Side Effects** | Transitions Lock to Released. Persists to TaskStore. |
| **v3 Source** | `handle_lock_release`, `release_agent_locks` |

### Domain 11: Scoring (1 primitive)

#### `compute-score`

Computes a composite quality score for a completed plan.

| Field | Value |
|-------|-------|
| **Params** | `store-path` (string: path to TaskStore), `duration-secs` (u64) |
| **Outputs** | `score` (object: {composite, completion, quality, efficiency, validation}) |
| **Preconditions** | TaskStore has data. |
| **Side Effects** | None (pure computation from stored data). |
| **v3 Source** | `scorer::compute` |

### Domain 12: Git Audit (3 primitives)

#### `audit-branches`

Verifies every non-terminal Bundle still has its agent branch.

| Field | Value |
|-------|-------|
| **Params** | (none) |
| **Outputs** | `mismatches` (array: [{bundle-id, expected-branch}]), `catastrophic` (bool) |
| **Preconditions** | None. |
| **Side Effects** | May force-reject bundles with missing branches. |
| **v3 Source** | `audit_branches` |

#### `audit-tick-shas`

Verifies Published Tick SHAs are reachable from HEAD.

| Field | Value |
|-------|-------|
| **Params** | (none) |
| **Outputs** | `unreachable` (array: [{tick-id, sha}]), `catastrophic` (bool) |
| **Preconditions** | None. |
| **Side Effects** | Sets stores.degraded flag if catastrophic. Emits `reconciliation_failed` event. |
| **v3 Source** | `audit_tick_shas` |

#### `audit-merge-ancestry`

Verifies merged Bundle commits are ancestors of their Tick's integration SHA.

| Field | Value |
|-------|-------|
| **Params** | (none) |
| **Outputs** | `broken` (array: [{bundle-id, tick-id}]), `catastrophic` (bool) |
| **Preconditions** | None. |
| **Side Effects** | Sets stores.degraded flag if catastrophic. Emits `reconciliation_failed` event. |
| **v3 Source** | `audit_merge_ancestry` |

### Domain 13: Conflict Resolution (1 primitive)

#### `combine-conflicting-works`

Combines multiple Works that touched overlapping files into a single replacement Work.

| Field | Value |
|-------|-------|
| **Params** | `work-ids` (string[]: conflicting work IDs), `conflicting-files` (string[]) |
| **Outputs** | `combined-work-id` (string) |
| **Preconditions** | All work IDs exist. At least 2 works. |
| **Side Effects** | Creates combined Work (union of titles, ACs, deps). Abandons original Works. Rewires sibling dependencies to point at new Work. Creates Learning (STRUCTURAL CONFLICT RESOLVED). Persists to TaskStore. |
| **v3 Source** | `combine_conflicting_works` in integrator |

### Domain 14: Events (1 primitive)

#### `emit-event`

Emits an arbitrary event to the daemon's event bus.

| Field | Value |
|-------|-------|
| **Params** | `event-type` (string), `payload` (object, optional) |
| **Outputs** | (none) |
| **Preconditions** | None. |
| **Side Effects** | Broadcasts DaemonEvent. Consumed by TUI, strategies, and any subscribers. |
| **v3 Source** | Various `event_tx.send()` calls throughout v3. In v3 these are implicit side effects of other operations; in v4 strategies may need to emit events directly. |

### Summary Table

| Domain | Primitives | Pure Queries | Side-Effecting |
|--------|-----------|-------------|----------------|
| Agent Lifecycle | 5 | 0 | 5 |
| Work Management | 7 | 0 | 7 |
| Record CRUD | 5 | 2 | 3 |
| Bundle Operations | 3 | 0 | 3 |
| Decomposition | 7 | 1 | 6 |
| Integration | 7 | 0 | 7 |
| Worktree | 4 | 0 | 4 |
| Context & Learning | 5 | 3 | 2 |
| Reconciliation & Escalation | 8 | 3 | 5 |
| Lock Management | 2 | 0 | 2 |
| Scoring | 1 | 1 | 0 |
| Git Audit | 3 | 0 | 3 |
| Conflict Resolution | 1 | 0 | 1 |
| Events | 1 | 0 | 1 |
| **Total** | **59** | **10** | **49** |

**Idempotency summary:** Of 59 primitives, ~10 are pure queries (Idempotent by nature), ~35 are state transitions (Idempotent - return Unchanged on re-call), ~8 are create-* operations (GuardRequired - check existence before calling), and ~6 are git-mutating (GuardRequired or NonIdempotent - protected by integrate-tick or advisory locks).

### What's NOT a Primitive (v3 Compositions -> v4 Strategies)

These v3 operations are multi-step sequences that become strategy compositions in YAML. This table is the bridge between "what v3 does" and "how v4 expresses it." Each row shows a v3 operation, why it fails the atomicity test, and which primitives a YAML strategy would compose to reproduce it:

| v3 Operation | Why It's a Composition | Primitives It Uses |
|-------------|----------------------|-------------------|
| Coordinator tick iteration | Multi-step: sweep, reconcile, LLM call, parse, execute actions | sweep-to-done, sweep-stuck-inreview, promote-record, complete-record, build-context, (LLM call is agent execution, not a primitive) |
| Integrator run_cycle | Full tick lifecycle: create, merge, validate, publish | create-tick, merge-branches, run-validation, transition-record |
| Reconciliation loop | Fixed-point loop of 5 promotion/completion passes | promote-record (x3), complete-record (x2), detect-goal-complete |
| Work assignment flow | Claim + transition + spawn | claim-next-work, transition-work, spawn-agent |
| Bundle rejection cascade | Reject bundle + reset work + rebase branch | reject-bundle (which internally calls reset-work), refresh-worktree |
| Conflict resolution | Combine works + abandon originals + rewire deps | create-work, override-work (xN), update-record (xN), create-learning |
| Recovery from stuck tick | Transition tick + create learning | transition-record, create-learning |
| Full decomposition pipeline | Classify tier + decompose (xN levels) + validate + ratify | classify-tier, decompose (xN), validate-document (xN), ratify-hierarchy |
| Re-decomposition after failure | Abandon children + re-decompose with new context | re-decompose (which internally calls abandon-children + decompose) |
| Abandon-ratio quality gate | Check ratio + escalate or proceed | check-ratio, escalate OR merge-integration-to-main |

### Implementation Plan

#### Phase 1: Trait and Registry

1. Define `Primitive` trait, `PrimitiveOutput`, `PrimitiveContext` in `src/primitive/mod.rs`
2. Implement `PrimitiveRegistry` with register/get/validate_references
3. Write startup validation: all YAML-referenced primitives must exist in registry

#### Phase 2: Pure Query Primitives

Implement the 11 pure-query primitives first (no side effects, easy to test):
- `query-records`, `get-record`, `detect-goal-complete`, `check-threshold`, `check-ratio`, `classify-tier`, `select-learnings`, `compact-context`, `compute-score`, `audit-tick-shas`, `audit-merge-ancestry`

#### Phase 3: Record Mutation Primitives

Implement the core CRUD and transition primitives:
- `create-record`, `update-record`, `transition-record`, `create-work`, `transition-work`, `override-work`, `create-bundle`, `create-tick`, `create-learning`

#### Phase 4: Agent and Worktree Primitives

Implement agent lifecycle and git operations:
- `spawn-agent`, `stop-agent`, `pause-agent`, `resume-agent`, `inject-context`
- `create-worktree`, `cleanup-worktree`, `delete-agent-branch`, `refresh-worktree`

#### Phase 5: Integration and Complex Primitives

Implement the remaining side-effecting primitives:
- `merge-branches`, `run-validation`, `create-integration-branch`, `merge-integration-to-main`, `delete-integration-branch`
- `decompose`, `validate-document`, `evaluate-coverage`, `ratify-hierarchy`
- `promote-record`, `complete-record`, `sweep-to-done`, `sweep-stuck-inreview`, `escalate`
- `claim-next-work`, `reset-work`, `reject-bundle`, `supersede-bundles`
- `increment-failure-count`, `increment-attempt-count`
- `acquire-lock`, `release-lock`
- `build-context`, `audit-branches`

## Alternatives Considered

### Alternative 1: Fewer, coarser primitives (combine related operations)

- **Description:** Merge related primitives (e.g., all Work operations into one `manage-work` primitive with an `action` parameter).
- **Pros:** Smaller registry. Fewer names to learn.
- **Cons:** Loses composability. A strategy can't wire "increment failure count" separately from "transition work." The `action` parameter becomes an inner dispatch that YAML can't reason about.
- **Why not chosen:** Fine-grained primitives are the whole point. YAML strategies compose small building blocks. Coarse primitives push composition back into Rust.

### Alternative 2: More, finer primitives (split everything)

- **Description:** Split every field mutation into its own primitive (e.g., `set-work-assignee`, `set-work-status`, `set-work-attempt-count`).
- **Pros:** Maximum granularity. Every field is independently wirable.
- **Cons:** Explosion of primitives (~150+). Most strategies would need 5-step sequences for what should be one operation. The abstraction level drops below useful composition.
- **Why not chosen:** The atomicity test catches this - "set one field" is too fine-grained because strategy authors never need to set assignee without also transitioning status. The right granularity is "one meaningful side effect."

### Alternative 3: Expression-based primitives (primitives that evaluate expressions)

- **Description:** Instead of `check-threshold` and `check-ratio` as separate primitives, have a single `evaluate-expression` primitive that takes a mini-DSL.
- **Pros:** Fewer primitives. More flexible condition checking.
- **Cons:** The mini-DSL grows toward Turing-completeness (explicit non-goal). Type safety at the expression boundary is hard. Error messages become opaque.
- **Why not chosen:** Design principle 6: "Composition, not scripting." Specific, named primitives are clearer than expression evaluation.

## Technical Considerations

### Dependencies

- **Internal:** TaskStore (Record trait), domain types, IPC bridge, WorktreeManager, LLM client
- **External:** tokio (async execution), serde_json (param/output serialization), eyre (error handling)

### Performance

- Primitive execution is dominated by I/O (git operations, LLM calls, filesystem). The registry lookup is a single HashMap access - negligible.
- Pure query primitives should be fast (<1ms). Git operations are 10-100ms. LLM calls are seconds.
- The `decompose` primitive is the most expensive (multiple LLM calls for hierarchy + validation + ratification).

### Security

- Primitives access the filesystem and execute shell commands only through established channels (WorktreeManager, ToolExecutor).
- The `run-validation` primitive executes arbitrary shell commands - same sandboxing as v3's validation commands.
- Primitive params are validated at startup (schema check) and at execution time (precondition check).

### Testing Strategy

- **Unit tests per primitive:** Each primitive tested in isolation with injected fakes (MockStores, MockBridge, MockWorktreeManager). Same pattern as v3.
- **Registry tests:** Validate that all primitives register correctly, that name conflicts are caught, and that missing-reference validation works.
- **Integration tests:** Wire primitives through the composition engine, verify correct execution order and output passing.

### Rollout Plan

- Implement phases 1-5 sequentially on the v4 branch
- Each phase must pass `otto ci` before proceeding
- Phase 1 (trait + registry) is the foundation - nothing else can start until it compiles
- Pure query primitives (phase 2) can be tested immediately against v3's data
- The full primitive set is complete when all 53 primitives are registered and unit-tested

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Missing primitives discovered during strategy authoring | High | Medium | The catalog is derived from exhaustive v3 audit, but edge cases will surface. Adding a new primitive is a small, well-defined task. |
| Primitive granularity wrong (too coarse or too fine) | Medium | Medium | The atomicity test provides a principled decision framework. Adjust during Doc 5 (strategy composition) when we see how primitives compose. |
| PrimitiveContext grows too large (too many fields) | Medium | Low | Context is a reference struct, not cloned. Add fields as needed. Could split into sub-contexts if it gets unwieldy. |
| Async primitives complicate the trait | Low | Low | Already addressed: trait returns `Pin<Box<dyn Future>>`. All primitives are async even if some complete synchronously. |
| Strategy-scoped context (HashMap) becomes a dumping ground | Medium | Medium | Document conventions: keys are `{primitive-name}.{output-name}`. Strategy context is cleared per-strategy, preventing cross-strategy pollution. |

## Resolved Questions

- [x] **QueryPrimitive trait split?** No. Start with one `Primitive` trait. A breadcrumb comment on the trait documents the future enhancement path (split into Primitive + QueryPrimitive for concurrent query execution). Split when query parallelism becomes a measurable bottleneck, not before.
- [x] **`reject-bundle` as primitive?** Yes. Bundle rejection + work reset is an invariant, not a composition choice. Splitting them is an error-prone footgun (orphaned works stuck in non-Ready state). Keeps it as a primitive.
- [x] **`decompose` granularity?** Keep it coarse. "Decompose a parent into children" is the right abstraction level. No strategy would call the LLM but skip validation. Added `re-decompose` and `abandon-children` as new primitives to handle the re-decomposition-after-new-knowledge use case, which IS a meaningful composition point.
- [x] **Registry aliases?** No aliases in the Rust registry. Handle convenience names (e.g., `abandon-work` for `override-work(target-status=abandoned)`) as default-param templates in the YAML composition layer. Keeps Rust simple, puts sugar where strategy authors work.

## References

- `docs/v4-vision.md` - v4 architecture vision (parent document)
- `docs/v4-architecture-sketch.md` - pre-design-doc thinking
- `docs/hardcoded-knobs-inventory.md` - every hardcoded parameter in v3
- `src/agents/coordinator/` - v3 coordinator (primary source for reconciliation, sweeps, quality gates)
- `src/agents/executor/` - v3 executor (primary source for agent lifecycle, action handlers)
- `src/daemon/handlers/` - v3 daemon handlers (primary source for record CRUD, transitions)
- `src/decomposer.rs` - v3 decomposer (primary source for decomposition primitives)
- `src/agents/integrator.rs` - v3 integrator (primary source for integration, git audit)
- `src/worktree/manager.rs` - v3 worktree management
- `src/agents/context.rs` - v3 context builder
- `src/agents/generation.rs` - v3 generation module
- `src/evaluator.rs` - v3 coverage evaluator
- `src/validator.rs` - v3 document validator
- `src/scorer.rs` - v3 scorer
