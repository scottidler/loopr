# Design Document: v4 FSM-in-YAML

**Author:** Scott A. Idler
**Date:** 2026-04-11
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

This document defines the YAML schema for FSM definitions and a runtime interpreter that replaces v3's `#[derive(Fsm)]` proc macro. FSM shapes, transitions, guards, role authorization, and overrides move from compile-time codegen to runtime interpretation of YAML files loaded at startup. The interpreter must pass v3's 128-test FSM suite as the baseline correctness bar.

## Problem Statement

### Background

v3 defines FSM rules via `#[derive(Fsm)]` attributes on status enums (WorkStatus, BundleStatus, HierarchyStatus, TickStatus). The proc macro in `loopr-derive` generates `validate_transition()`, `validate_override()`, and `is_terminal()` methods at compile time. This provides type safety but locks transition rules into Rust source code - changing a transition requires a code change, recompile, and redeploy.

v3 has 5 FSMs:
- **WorkStatus** (9 states, 2 override edges, 3 roles)
- **BundleStatus** (8 states, no overrides, 3 roles)
- **HierarchyStatus** (5 states, shared by Plan/Spec/Phase, Coordinator-only)
- **TickStatus** (5 states, Integrator-only)
- **AgentStatus** (8 states, non-domain agent lifecycle, hand-written `can_transition_to()` without role authorization - NOT derived with `#[derive(Fsm)]`)

LockStatus is NOT an FSM - it uses imperative methods without role-based authorization. AgentStatus is a borderline case: it has transition rules but no role authorization and no derive macro. The YAML definition captures its transitions with empty `by` lists (any caller can transition).

### Problem

FSM transition rules are baked into Rust enums. AR cannot explore alternative state machines (e.g., adding a "needs-review" state to the hierarchy, or an "advisory" intermediate state for bundles) without modifying Rust code. The v4 vision requires FSMs to be YAML-defined so strategies can reference and compose them.

### Goals

- YAML schema that captures: states, transitions (with role authorization), overrides (with role authorization), terminal states, and guards
- Runtime FSM interpreter that loads YAML at startup and enforces transitions at runtime
- Startup validation: orphan states, unreachable states, guard references resolve, terminals have no outgoing transitions
- Idempotent self-transitions (from == target returns Unchanged, matching v3 behavior)
- Error messages as clear as v3's compile-time errors
- v3's 128-test FSM suite passes against the runtime interpreter
- Domain types reference their FSM by name (not by enum attribute)

### Non-Goals

- FSM inheritance or composition (decided in vision doc: keep flat, one file per domain type)
- Hot-reloading FSMs mid-run (loaded at startup, fixed for the run)
- Replacing LockStatus with an FSM (it works fine as imperative methods)
- Guard implementation details (covered in Doc 4: Triggers and Guards)

## Proposed Solution

### Overview

The solution has three parts:

1. **YAML schema** - declares FSM definitions in `strategies/fsm/*.yml`
2. **Schema parser + validator** - loads YAML at startup, validates structure, produces typed `FsmDefinition` structs
3. **Runtime interpreter** - holds loaded definitions, answers "is this transition valid?" queries at runtime

### YAML Schema

Each FSM is defined in a single YAML file. The schema captures everything v3's `#[derive(Fsm)]` attributes express, plus guards (for Doc 4).

```yaml
# strategies/fsm/work.yml
name: work
description: Work item lifecycle - from creation to completion or abandonment

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
    blocked: {}                                # any role (empty by = unrestricted)
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

# Guards are evaluated by the trigger system (Doc 4).
# Listed here for documentation; enforcement is in the engine.
guards: {}
```

**Key schema decisions:**

- **`transitions` is a map-of-maps** (source state -> target state -> rule). The outer key is the source state, the inner key is the target state, the value is the rule (currently just `by`). This follows the keyed-map convention: the target state IS the key, eliminating the redundant `to` field, giving O(1) lookup by target, and making duplicate targets a parse error rather than a silent bug.
- **`by` is optional.** Omitting it (or empty `{}`) means any role can trigger the transition. This matches v3's `InProgress -> Blocked` (no role restriction).
- **`overrides` is a separate top-level key**, not mixed into transitions. This preserves v3's two-method pattern: `validate_transition()` checks transitions only; `validate_override()` checks transitions + overrides. The interpreter implements both.
- **States use kebab-case** in YAML, matching the general naming convention. The interpreter maps between kebab-case YAML names and PascalCase Rust enum variants.
- **Terminal states don't appear as keys in `transitions`** - they have no outgoing edges. Startup validation enforces this: if a terminal state appears as a transitions key, it's an error.
- **`guards` is a placeholder** for Doc 4. Listed in the schema now so the YAML structure is stable.

### All Five FSM Definitions

**Bundle:**
```yaml
# strategies/fsm/bundle.yml
name: bundle
description: Bundle lifecycle - from proposal through review to merge or rejection

states:
  - proposed
  - triaged
  - reviewed
  - accepted
  - integrating
  - merged
  - rejected
  - superseded

terminal:
  - merged
  - rejected
  - superseded

transitions:
  proposed:
    triaged: { by: [coordinator] }
    rejected: { by: [coordinator, reviewer] }
    superseded: { by: [coordinator] }
  triaged:
    reviewed: { by: [coordinator, reviewer] }
    accepted: { by: [coordinator] }
    rejected: { by: [coordinator, reviewer] }
    superseded: { by: [coordinator] }
  reviewed:
    accepted: { by: [coordinator] }
    rejected: { by: [coordinator, reviewer] }
    superseded: { by: [coordinator] }
  accepted:
    integrating: { by: [integrator] }
    rejected: { by: [integrator] }
    superseded: { by: [coordinator] }
  integrating:
    merged: { by: [integrator] }
    rejected: { by: [integrator] }
    superseded: { by: [coordinator] }

overrides: {}

guards: {}
```

**Hierarchy (shared by Plan, Spec, Phase):**
```yaml
# strategies/fsm/hierarchy.yml
name: hierarchy
description: Shared lifecycle for Plan, Spec, and Phase records

states:
  - draft
  - pending
  - active
  - complete
  - abandoned

terminal:
  - complete
  - abandoned

transitions:
  draft:
    pending: { by: [coordinator] }
    active: { by: [coordinator] }
    abandoned: { by: [coordinator] }
  pending:
    active: { by: [coordinator] }
    abandoned: { by: [coordinator] }
  active:
    complete: { by: [coordinator] }
    abandoned: { by: [coordinator] }

overrides: {}

guards: {}
```

**Tick:**
```yaml
# strategies/fsm/tick.yml
name: tick
description: Tick lifecycle - integration batch from open to published or failed

states:
  - open
  - sealing
  - validating
  - published
  - failed

terminal:
  - published
  - failed

transitions:
  open:
    sealing: { by: [integrator] }
    failed: { by: [integrator] }
  sealing:
    validating: { by: [integrator] }
    failed: { by: [integrator] }
  validating:
    published: { by: [integrator] }
    failed: { by: [integrator] }

overrides: {}

guards: {}
```

**Agent:**
```yaml
# strategies/fsm/agent.yml
name: agent
description: Agent session lifecycle

# Note: v3's AgentStatus uses hand-written can_transition_to() without role
# authorization (no #[derive(Fsm)]). The YAML definition omits "by" on all
# transitions, matching v3's "any caller can transition" behavior.

states:
  - starting
  - running
  - waiting-for-llm
  - paused
  - idle
  - completed
  - failed
  - cancelled

terminal:
  - completed
  - failed
  - cancelled

transitions:
  starting:
    running: {}
    failed: {}
    cancelled: {}
  running:
    waiting-for-llm: {}
    paused: {}
    idle: {}
    completed: {}
    failed: {}
    cancelled: {}
  waiting-for-llm:
    running: {}
    failed: {}
    cancelled: {}
  paused:
    running: {}
    cancelled: {}
  idle:
    running: {}
    cancelled: {}

overrides: {}

guards: {}
```

### Data Model

```rust
/// Authorization rule for a transition to a target state.
/// The target state name is the YAML map key, injected into the `name` field
/// via a custom serde Visitor (the keyby pattern from otto/aka).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TransitionRule {
    /// Target state name - populated from the YAML map key, not from a YAML field.
    #[serde(skip_deserializing)]
    pub name: String,
    /// Roles authorized to perform this transition.
    /// Empty vec means any role is allowed.
    #[serde(default)]
    pub by: Vec<String>,
}

/// A guard condition on a transition (placeholder for Doc 4).
#[derive(Debug, Clone, Deserialize)]
pub struct GuardDef {
    pub from: String,
    pub to: String,
    pub condition: String,
}

/// Complete FSM definition loaded from YAML.
#[derive(Debug, Clone, Deserialize)]
pub struct FsmDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub states: Vec<String>,
    pub terminal: Vec<String>,
    /// Source state -> (target state -> rule).
    /// Keyed-map form: target state IS the key, not a field.
    pub transitions: HashMap<String, HashMap<String, TransitionRule>>,
    /// Source state -> (target state -> rule) for override-only edges.
    #[serde(default)]
    pub overrides: HashMap<String, HashMap<String, TransitionRule>>,
    #[serde(default)]
    pub guards: HashMap<String, GuardDef>,
}
```

### Runtime Interpreter

```rust
/// Holds all loaded FSM definitions. Immutable after startup.
pub struct FsmInterpreter {
    definitions: HashMap<String, FsmDefinition>,
}

impl FsmInterpreter {
    /// Load all FSM definitions from a directory.
    pub fn load(dir: &Path) -> eyre::Result<Self> { /* ... */ }

    /// Strongly-typed transition validation. Call sites use domain types,
    /// not raw strings. The FsmStatus trait handles kebab-case mapping.
    pub fn validate<S: FsmStatus>(
        &self,
        from: &S,
        to: &S,
        role: &str,
    ) -> eyre::Result<Transition> {
        self.validate_transition(S::fsm_name(), from.to_yaml_name(), to.to_yaml_name(), role)
    }

    /// Strongly-typed override validation.
    pub fn validate_override_typed<S: FsmStatus>(
        &self,
        from: &S,
        to: &S,
        role: &str,
    ) -> eyre::Result<Transition> {
        self.validate_override(S::fsm_name(), from.to_yaml_name(), to.to_yaml_name(), role)
    }

    /// Raw string validation (used by the engine for YAML-driven transitions).
    /// Returns Changed if valid, Unchanged if from == to (idempotent),
    /// or Err if the transition is invalid.
    ///
    /// Design decision: self-transitions (from == to) bypass role authorization
    /// and return Unchanged. This matches v3 behavior and is intentional for
    /// idempotency - a handler that transitions a record to its current state
    /// should succeed silently regardless of the caller's role.
    pub fn validate_transition(
        &self,
        fsm_name: &str,
        from: &str,
        to: &str,
        role: &str,
    ) -> eyre::Result<Transition> {
        if from == to {
            return Ok(Transition::Unchanged);
        }
        let def = self.get_definition(fsm_name)?;
        let targets = def.transitions.get(from)
            .ok_or_else(|| invalid_transition(fsm_name, from, to, role, None))?;
        self.check_target(targets, to, role, fsm_name, from)
    }

    /// Validate an override transition.
    /// Checks normal transitions first, then override edges.
    /// Preserves error context from the normal transition attempt.
    pub fn validate_override(
        &self,
        fsm_name: &str,
        from: &str,
        to: &str,
        role: &str,
    ) -> eyre::Result<Transition> {
        // Try normal transition first
        match self.validate_transition(fsm_name, from, to, role) {
            Ok(result) => return Ok(result),
            Err(normal_err) => {
                // Then check overrides, preserving normal error as context
                let def = self.get_definition(fsm_name)?;
                let targets = def.overrides.get(from)
                    .ok_or_else(|| invalid_transition(
                        fsm_name, from, to, role,
                        Some(&format!("normal transition also failed: {}", normal_err)),
                    ))?;
                self.check_target(targets, to, role, fsm_name, from)
            }
        }
    }

    /// Check if a state is terminal.
    pub fn is_terminal(&self, fsm_name: &str, state: &str) -> eyre::Result<bool> {
        let def = self.get_definition(fsm_name)?;
        Ok(def.terminal.iter().any(|t| t == state))
    }

    /// Get all valid target states from a given state (for context/prompt building).
    pub fn valid_targets(
        &self,
        fsm_name: &str,
        from: &str,
        role: &str,
    ) -> eyre::Result<Vec<String>> { /* ... */ }

    fn get_definition(&self, name: &str) -> eyre::Result<&FsmDefinition> {
        self.definitions.get(name)
            .ok_or_else(|| eyre::eyre!("unknown FSM: {}", name))
    }

    fn check_target(
        &self,
        targets: &HashMap<String, TransitionRule>,
        to: &str,
        role: &str,
        fsm_name: &str,
        from: &str,
    ) -> eyre::Result<Transition> {
        // O(1) lookup by target state - the keyed-map payoff
        match targets.get(to) {
            Some(rule) if rule.by.is_empty() || rule.by.iter().any(|r| r == role) => {
                Ok(Transition::Changed)
            }
            Some(_) => {
                // Right target, wrong role
                Err(invalid_transition(fsm_name, from, to, role, None))
            }
            None => {
                Err(invalid_transition(fsm_name, from, to, role, None))
            }
        }
    }
}

fn invalid_transition(
    fsm: &str, from: &str, to: &str, role: &str, context: Option<&str>,
) -> eyre::Report {
    let base = format!("invalid {} transition: {} -> {} (role: {})", fsm, from, to, role);
    match context {
        Some(ctx) => eyre::eyre!("{}\n  {}", base, ctx),
        None => eyre::eyre!("{}", base),
    }
}
```

### Startup Validation

The schema parser validates every FSM definition at load time:

| Check | What It Catches |
|-------|----------------|
| **All terminal states are listed in states** | Typo in terminal list |
| **All transition sources are listed in states** | Transition from nonexistent state |
| **All transition targets are listed in states** | Transition to nonexistent state |
| **Terminal states have no outgoing transitions** | Contradictory definition |
| **All override sources are listed in states** | Override from nonexistent state |
| **All override targets are listed in states** | Override to nonexistent state |
| **All roles are valid Role enum variants** | Typo in role name |
| **No duplicate transition rules (same from+to)** | Ambiguous authorization |
| **At least one terminal state exists** | FSM that can never finish |
| **All non-terminal states can reach a terminal** | Unreachable/orphan states |
| **Guard condition names resolve** | Reference to nonexistent primitive/query (Doc 4) |
| **FSM name matches filename** | `work.yml` must define `name: work` |

Validation errors are collected and reported as a batch (not fail-on-first), so the user sees all problems at once.

### Name Mapping: kebab-case YAML to PascalCase Rust

**States** and **roles** both need mapping between YAML (kebab-case) and Rust (PascalCase):
- States: `in-progress` <-> `InProgress`, `waiting-for-llm` <-> `WaitingForLlm`
- Roles: `coordinator` <-> `Coordinator`, `integrator` <-> `Integrator`

Role mapping is simpler (all single words), but the interpreter should validate role names against the Role enum at startup. An `{ to: ready, by: [cordinator] }` typo must be caught before any work starts.

The interpreter works with kebab-case strings internally. Domain types that carry status fields need a mapping layer:

```rust
/// Convert between kebab-case YAML names and PascalCase Rust enum variants.
/// "in-progress" <-> WorkStatus::InProgress
pub trait FsmStatus: Sized {
    /// The FSM definition name this status belongs to.
    fn fsm_name() -> &'static str;
    /// Convert to kebab-case YAML name.
    fn to_yaml_name(&self) -> &'static str;
    /// Parse from kebab-case YAML name.
    fn from_yaml_name(name: &str) -> eyre::Result<Self>;
}
```

Each status enum implements `FsmStatus`. This can be derived (a simple attribute macro) or hand-written - it's mechanical and changes rarely.

**Data migration note:** v3 uses `#[serde(rename_all = "lowercase")]` on `HierarchyStatus`, which serializes `InProgress` as `"inprogress"` in JSONL/SQLite. The YAML FSM definitions use kebab-case (`"in-progress"`). These are two different mappings. The `FsmStatus` trait maps between PascalCase Rust and kebab-case YAML for FSM validation only. Serde serialization to JSONL/SQLite continues to use the v3 lowercase format. The two formats do not need to match - `FsmStatus::to_yaml_name()` is for FSM interpreter queries, `serde::Serialize` is for persistence. No data migration required.

### Interpreter Ownership

The `FsmInterpreter` is immutable after startup. It is owned by the daemon and passed by shared reference (`&FsmInterpreter`) to the composition engine, primitives, and handlers. Since it's read-only, no locking is needed.

```rust
// In daemon startup:
let interpreter = FsmInterpreter::load(strategies_dir.join("fsm"))?;
// Passed to engine, primitives, handlers as &interpreter
```

### How Domain Types Reference Their FSM

In v3, domain types embed their FSM via `#[derive(Fsm)]` on the status enum. In v4, the link is by name:

```rust
// v3: compile-time FSM
#[derive(Fsm)]
pub enum WorkStatus {
    #[transitions(Pending(Coordinator), Ready(Coordinator), Abandoned(Coordinator))]
    Draft,
    // ...
}

// v4: runtime FSM
pub enum WorkStatus {
    Draft, Pending, Ready, InProgress, Blocked, InReview, Integrated, Done, Abandoned,
}
impl FsmStatus for WorkStatus {
    fn fsm_name() -> &'static str { "work" }
    // ...
}

// Transition validation goes through the interpreter:
interpreter.validate_transition("work", "draft", "ready", "coordinator")?;
```

The `validate_transition` and `validate_override` methods that v3 generates on the enum are replaced by calls to the interpreter. The domain types become pure data enums - no FSM logic.

### Migration Path

The migration is straightforward because both systems answer the same question ("is this transition valid?"):

1. **Strip `#[derive(Fsm)]`** from all status enums. Keep `#[derive(FlexibleEnum)]` (still needed for serde).
2. **Implement `FsmStatus`** on each status enum (mechanical: map PascalCase variants to kebab-case names).
3. **Write YAML definitions** for all 5 FSMs (done above in this doc).
4. **Replace `status.validate_transition(target, role)`** with `interpreter.validate_transition(fsm_name, from, to, role)` at every call site.
5. **Replace `status.validate_override(target, role)`** with `interpreter.validate_override(...)`.
6. **Replace `status.is_terminal()`** with `interpreter.is_terminal(fsm_name, state)`.
7. **Port v3's 128 FSM tests** to call the interpreter instead of the derive-generated methods. Tests should be structurally identical - only the call site changes.
8. **Remove `Fsm` derive from `loopr-derive`** once all tests pass against the interpreter.

Steps 1-3 can happen in parallel. Step 4-6 is mechanical find-and-replace. Step 7 is the validation gate. Step 8 is cleanup.

### Error Messages

v3's derive-generated errors look like:
```
LooprError::InvalidTransition { from: "InProgress", to: "Done", role: "Implementer" }
```

v4's interpreter errors must be at least as informative:
```
invalid work transition: in-progress -> done (role: implementer)
  hint: valid targets from in-progress: blocked (any), in-review (implementer), abandoned (coordinator)
  hint: with overrides: ready (coordinator), in-review (coordinator)
```

The interpreter can provide hints because it has the full transition map at runtime - something the compile-time macro couldn't do.

### Implementation Plan

#### Phase 1: Data Model and Parser

1. Create `src/fsm/mod.rs` with `FsmDefinition`, `TransitionRule`, `GuardDef` structs
2. Create `src/fsm/schema.rs` with YAML parsing and startup validation
3. Write YAML files for all 5 FSMs in `strategies/fsm/`
4. Unit tests: parsing valid YAML, rejecting invalid YAML, all validation checks

#### Phase 2: Runtime Interpreter

1. Create `src/fsm/runtime.rs` with `FsmInterpreter`
2. Implement `validate_transition`, `validate_override`, `is_terminal`, `valid_targets`
3. Port v3's FSM tests to call the interpreter
4. Verify all 128 tests pass (minus Lock tests which aren't FSM-based)

#### Phase 3: Domain Integration

1. Implement `FsmStatus` trait and impls for WorkStatus, BundleStatus, HierarchyStatus, TickStatus, AgentStatus
2. Replace `#[derive(Fsm)]` with `FsmStatus` impl on each enum
3. Find-and-replace all `validate_transition` / `validate_override` / `is_terminal` call sites
4. Remove `Fsm` derive from `loopr-derive`
5. `otto ci` passes

## Alternatives Considered

### Alternative 1: Keep `#[derive(Fsm)]` alongside runtime interpreter

- **Description:** Leave the derive macro as a compile-time validation layer, add the runtime interpreter for YAML-defined FSMs used by strategies.
- **Pros:** Compile-time safety preserved for existing domain types. Only new/experimental FSMs use runtime interpretation.
- **Cons:** Two sources of truth. If YAML and derive disagree, which wins? Maintenance burden of keeping both in sync. Defeats the purpose of YAML-driven FSMs.
- **Why not chosen:** The whole point is one source of truth. Having two is worse than having either one alone.

### Alternative 2: Code-generate Rust from YAML (build.rs)

- **Description:** Use a build.rs script to read YAML at compile time and generate the same `validate_transition` match arms the derive macro produces.
- **Pros:** Compile-time safety. No runtime interpretation overhead.
- **Cons:** Still requires recompilation when YAML changes. AR can't load trial FSMs at runtime. The build.rs adds complexity and makes the build non-trivial. Doesn't achieve the v4 goal of YAML-as-the-experimentation-surface.
- **Why not chosen:** The overhead of runtime interpretation is negligible (one HashMap lookup per transition). The flexibility of runtime loading is the entire point.

### Alternative 3: Serialize the derive macro's output to YAML (reverse direction)

- **Description:** Keep `#[derive(Fsm)]`, but generate YAML files from the Rust enums as documentation.
- **Pros:** Rust remains the source of truth. YAML is always in sync.
- **Cons:** YAML becomes read-only documentation, not the configuration surface. AR still can't modify FSMs. Inverts the intended direction of control.
- **Why not chosen:** v4's architectural principle is "YAML defines behavior, Rust interprets it." This alternative goes the wrong direction.

## Technical Considerations

### Dependencies

- **Internal:** Domain types (status enums), `Transition` enum, `Role` enum, `LooprError::InvalidTransition`
- **External:** `serde_yaml` (parsing), `serde` (deserialization), `eyre` (error handling)
- **keyby crate (to be created):** The keyed-map YAML pattern (map key injected as struct field via custom `Visitor` + `#[serde(skip_deserializing)]`) is hand-written boilerplate in otto and aka. This should become a small derive-macro crate (`keyby` or `serde-keyby`) that generates the custom deserializer from an attribute like `#[derive(KeyBy)] #[key_by(field = "name")]`. This is a prerequisite for v4 since every YAML config layer uses this pattern. See `otto-rs/otto/src/cfg/task.rs:520-546` and `scottidler/aka/src/cfg/spec.rs:40-108` for the existing pattern.

### Performance

- YAML parsing happens once at startup. Zero runtime parsing cost.
- Transition validation is one HashMap lookup (FSM name) + one HashMap lookup (from state) + one linear scan of targets (typically 2-4 entries). Total: sub-microsecond.
- v3's compile-time match arm is a single jump table lookup. The runtime interpreter is ~10x slower in absolute terms but still negligible relative to the I/O and LLM calls that dominate execution.

### Security

- YAML files are loaded from a known directory (`strategies/fsm/`), not arbitrary paths.
- The interpreter cannot execute code - it only answers "is this transition valid?"
- Role names are validated against the known Role enum at startup.

### Testing Strategy

- **Parser tests:** Valid YAML loads correctly. Invalid YAML (missing fields, unknown states, orphan states) produces clear errors.
- **Validation tests:** All 12 startup validation checks have positive and negative test cases.
- **Interpreter tests:** Port all 128 v3 FSM tests. Each test calls the interpreter instead of the derive-generated method. Same assertions, same expected outcomes.
- **Regression gate:** v3 test suite must pass with zero failures before the derive macro is removed.
- **Property tests (stretch goal):** Generate random transitions, verify interpreter agrees with derive-generated methods on the same inputs. This catches any behavioral divergence between YAML definitions and v3's Rust attributes.

### Rollout Plan

- Implement on v4 branch
- Phase 1 (parser + validation) is independent - can start immediately
- Phase 2 (interpreter) depends on Phase 1
- Phase 3 (domain integration) depends on Phase 2
- The `Fsm` derive macro is not removed until ALL tests pass against the interpreter
- During transition: both systems can coexist (derive methods still exist, interpreter is added alongside)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| YAML definitions don't exactly match v3 derive attributes | Medium | High | The 128-test suite is the correctness gate. Any divergence is caught immediately. |
| Error messages less helpful than compile-time errors | Medium | Medium | The interpreter can actually provide BETTER hints (valid targets, override edges) because it has the full map at runtime. |
| Performance regression from interpretation | Low | Low | Sub-microsecond per validation. Profile if suspected. |
| Name mapping bugs (kebab-case <-> PascalCase) | Medium | Low | The FsmStatus trait is mechanical. Unit test every variant mapping. |
| Missing validation check allows invalid YAML through | Medium | Medium | The 12 validation checks are comprehensive. Add more as edge cases surface. |
| AgentStatus FSM not captured in v3 audit | Low | Medium | Included in the YAML definitions above. Tests will catch any missed transitions. |

## Resolved Questions

- [x] **FSM inheritance/composition?** No. Decided in vision doc. Keep flat - one file per domain type.
- [x] **Hot-reload?** No. FSMs loaded at startup, fixed for the run.
- [x] **What about LockStatus?** Not an FSM. Stays as imperative methods. No YAML definition needed.
- [x] **kebab-case vs PascalCase in YAML?** kebab-case in YAML (consistent with all other YAML naming). FsmStatus trait handles the mapping. Serde persistence uses v3's lowercase format unchanged - no data migration needed.
- [x] **Why are states (nodes) fixed in Rust but transitions (edges) configurable in YAML?** By design. Domain types (WorkStatus, BundleStatus, etc.) are the carry-over layer - they define the TaskStore schema, TUI views, and IPC protocol. Adding a new state requires a new Rust enum variant, new TaskStore indexes, new TUI rendering, and new IPC handling. AR experiments with transitions, guards, strategies, and decomposition pipelines - not with new FSM states. The experimentation surface is the edges and the policies, not the nodes.

## Open Questions

- [ ] Should the `FsmStatus` trait be derived via a simple attribute macro, or hand-written? It's ~15 lines per enum, 5 enums = 75 lines. Hardly worth a macro, but it IS mechanical and error-prone.
- [ ] Should the interpreter return the same `LooprError::InvalidTransition` error type as v3, or a new FSM-specific error type? Using the same type eases migration but couples the interpreter to the error module.
- [ ] The `valid_targets` method is new (v3 doesn't have it). Should it be exposed through IPC for agent prompts, or is it only for internal engine use?

## References

- `docs/v4-vision.md` - v4 architecture vision
- `docs/design/2026-04-11-primitive-vocabulary.md` - primitive catalog (Doc 2)
- `loopr-derive/src/lib.rs` - v3 Fsm derive macro implementation
- `src/domain/work.rs` - WorkStatus enum with transitions/overrides
- `src/domain/bundle.rs` - BundleStatus enum
- `src/domain/plan.rs` - HierarchyStatus enum (shared by Plan, Spec, Phase)
- `src/domain/tick.rs` - TickStatus enum
- `src/domain/role.rs` - Role enum
- `src/domain/transition.rs` - Transition enum (Changed/Unchanged)
- `src/tests/fsm/` - v3 FSM test suite (128 tests)
