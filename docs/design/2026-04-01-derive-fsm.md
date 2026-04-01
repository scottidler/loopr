# Design Document: Derive FSM - Declarative State Machines via Proc Macro

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Replace hand-maintained transition rule arrays with a `#[derive(Fsm)]` proc macro that encodes valid transitions and role constraints directly on enum variants. The enum becomes the single source of truth for the FSM - transitions, roles, terminal states, and idempotency are all derived at compile time. This eliminates ~150 lines of boilerplate rule arrays, prevents drift between enum definitions and transition logic, and makes the state machine readable at a glance.

## Problem Statement

### Background

Loopr has 4 FSM enums validated through `validate_transition()`: `HierarchyStatus` (4 states, shared by Plan/Spec/Phase), `WorkStatus` (8 states), `BundleStatus` (8 states), and `TickStatus` (5 states). Each enum has a companion `_transitions()` function returning `Vec<TransitionRule<S>>` - hand-written arrays of `{ from, to, role }` triples. `WorkStatus` additionally has `override_transitions()` for coordinator overrides.

The existing `loopr-derive` crate already provides `#[derive(FlexibleEnum)]` for case-insensitive parsing and variant name constants.

### Problem

The transition rule arrays are:

1. **Disconnected from their enums.** The `BundleStatus` enum is defined in `bundle.rs:12-22`, but `bundle_transitions()` is at `bundle.rs:31-141`. Adding a new variant requires updating two separate locations. Nothing at the type level enforces that every non-terminal state has transitions defined, or that a transition references only valid variants.

2. **Verbose.** Each rule is 4 lines of struct initialization. `bundle_transitions()` alone is 110 lines for 20 rules. `work_transitions()` is 85 lines for 16 rules. The signal-to-noise ratio is low.

3. **Prone to silent drift.** When we added crash recovery paths to `TickStatus` (Open->Failed, Sealing->Failed), the transition rules had to be manually updated in a separate function. The compiler gives no warning if a transition function is incomplete or inconsistent with the enum.

4. **Idempotency was missing.** The `validate_transition` function rejected `from == to` transitions, causing coordinator death loops when agents retried transitions on already-transitioned records. This was added as a one-line fix but belongs in the type system, not as a special case in the validator.

### Goals

- Encode all FSM transition rules as attributes on enum variants
- Role constraints inline with transition targets - bare = any role, parenthesized = specific roles
- Override transitions (WorkStatus) as a separate attribute
- Idempotency (self-transitions succeed) baked into generated code
- `is_terminal()` derived from variants with no `#[transitions]` attribute
- Eliminate hand-written `_transitions()` functions and the `TransitionRule` type
- Generated `validate_transition` method replaces the free function
- Zero runtime cost difference - same match-based validation, just generated

### Non-Goals

- Changing the `Role` enum or adding new roles
- Modifying handler call sites beyond switching from `validate_transition(from, to, role, &rules)` to `from.validate_transition(to, role)`
- Encoding the `CoordinatorFsmState` enum (it doesn't use `validate_transition`)
- Compile-time transition verification (would require const generics or type-state pattern - too invasive)
- Generating handler boilerplate (event emission, error responses) - only validation

## Proposed Solution

### Overview

Add a `#[derive(Fsm)]` proc macro to `loopr-derive` that reads `#[transitions(...)]` and `#[overrides(...)]` attributes on enum variants and generates validation methods.

### Syntax

```rust
#[derive(Fsm)]
enum WorkStatus {
    #[transitions(Ready(Coordinator), Abandoned(Coordinator))]
    Draft,
    #[transitions(InProgress(Coordinator), Blocked(Coordinator), Abandoned(Coordinator))]
    Ready,
    #[transitions(
        Blocked,                   // any role
        InReview(Implementer),     // implementer only (normal path)
        Abandoned(Coordinator),
    )]
    #[overrides(
        Ready(Coordinator),        // reset stuck work for re-assignment
        InReview(Coordinator),     // force to review when bundle exists
    )]
    InProgress,
    #[transitions(Ready(Coordinator), Abandoned(Coordinator))]
    Blocked,
    #[transitions(InProgress(Coordinator), Integrated(Integrator), Abandoned(Coordinator))]
    #[overrides(Ready(Coordinator))]   // reset to Ready when no valid bundle
    InReview,
    #[transitions(Done(Coordinator, Integrator), Abandoned(Coordinator))]
    Integrated,
    Done,       // terminal - no #[transitions]
    Abandoned,  // terminal - no #[transitions]
}
```

**Syntax rules:**
- `Target` - any role may perform this transition
- `Target(Role1, Role2)` - only these roles may perform this transition
- No `#[transitions]` attribute = terminal state
- `#[overrides(...)]` = additional edges available only via the override path
- Role names must match `Role` enum variants exactly

### Generated Code

For each `#[derive(Fsm)]` enum, the macro generates:

```rust
impl WorkStatus {
    /// Validate a normal transition. Returns `Changed` if the transition
    /// is valid and moves to a new state, `Unchanged` if from == target
    /// (idempotent no-op), or `Err` if the transition is invalid.
    pub fn validate_transition(
        self,
        target: Self,
        role: crate::domain::role::Role,
    ) -> crate::error::Result<crate::domain::transition::Transition> {
        use crate::domain::role::Role;
        use crate::domain::transition::Transition;
        // Idempotent: same state is a valid no-op
        if self == target {
            return Ok(Transition::Unchanged);
        }
        let allowed = match (self, target) {
            (Self::Draft, Self::Ready) => matches!(role, Role::Coordinator),
            (Self::Draft, Self::Abandoned) => matches!(role, Role::Coordinator),
            (Self::Ready, Self::InProgress) => matches!(role, Role::Coordinator),
            // ... all transitions ...
            (Self::InProgress, Self::Blocked) => true, // any role
            _ => false,
        };
        if !allowed {
            return Err(crate::error::LooprError::InvalidTransition {
                from: format!("{:?}", self),
                to: format!("{:?}", target),
                role: role.to_string(),
            });
        }
        Ok(Transition::Changed)
    }

    /// Validate an override transition. Includes all normal transitions
    /// plus override-only edges.
    pub fn validate_override(
        self,
        target: Self,
        role: crate::domain::role::Role,
    ) -> crate::error::Result<crate::domain::transition::Transition> {
        // Try normal transition first
        if let Ok(result) = self.validate_transition(target, role) {
            return Ok(result);
        }
        // Check override-only edges
        use crate::domain::role::Role;
        use crate::domain::transition::Transition;
        let allowed = match (self, target) {
            (Self::InProgress, Self::Ready) => matches!(role, Role::Coordinator),
            (Self::InProgress, Self::InReview) => matches!(role, Role::Coordinator),
            // ... override edges ...
            _ => false,
        };
        if !allowed {
            return Err(crate::error::LooprError::InvalidTransition {
                from: format!("{:?}", self),
                to: format!("{:?}", target),
                role: role.to_string(),
            });
        }
        Ok(Transition::Changed)
    }

    /// True if this state has no outgoing transitions.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Abandoned)
    }
}
```

The `Transition` type is defined in `src/domain/transition.rs`:

```rust
/// Result of a validated state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The transition is valid and moves to a new state.
    Changed,
    /// The target state equals the current state (idempotent no-op).
    Unchanged,
}
```

Handlers use this to short-circuit DB writes and event broadcasts on `Unchanged`:

```rust
// Before (current code):
if let Err(e) = validate_transition(from, target_status, role, &rules) { ... }
// proceeds to write + broadcast unconditionally

// After:
match from.validate_transition(target_status, role)? {
    Transition::Unchanged => return Ok(DaemonResponse::ok(req.id, json!(null))),
    Transition::Changed => { /* proceed to write + broadcast */ }
}
```

### Architecture

```
Before:
  enum WorkStatus { ... }          // definition in work.rs
  fn work_transitions() -> Vec<>   // rules 60 lines below
  fn override_transitions() -> Vec<>
  fn validate_transition(...)      // generic free function in transition.rs
  handler: validate_transition(from, to, role, &work_transitions())

After:
  #[derive(Fsm)]
  enum WorkStatus {                // definition IS the rules
      #[transitions(...)]
      Draft,
      ...
  }
  handler: from.validate_transition(to, role)
```

**Files changed:**
- `loopr-derive/src/lib.rs` - add `#[derive(Fsm)]` proc macro
- `src/domain/plan.rs` - add attributes to `HierarchyStatus`, remove `hierarchy_transitions()`
- `src/domain/work.rs` - add attributes to `WorkStatus`, remove `work_transitions()` and `override_transitions()`
- `src/domain/bundle.rs` - add attributes to `BundleStatus`, remove `bundle_transitions()`
- `src/domain/tick.rs` - add attributes to `TickStatus`, remove `tick_transitions()`
- `src/domain/transition.rs` - add `Transition` enum (`Changed`/`Unchanged`), remove `validate_transition()` free function and `TransitionRule` struct
- `src/daemon/handlers/*.rs` - update call sites from `validate_transition(from, to, role, &rules)` to `from.validate_transition(to, role)` (or `from.validate_override(to, role)`)
- `src/fsm_correctness_tests.rs` - update tests (self-transitions now succeed, rule-based tests become attribute-based)

### Implementation Plan

#### Phase 1: Proc Macro

Add `#[derive(Fsm)]` to `loopr-derive`. Parse `#[transitions(...)]` and `#[overrides(...)]` attributes. Generate `validate_transition` (returning `Result<Transition>`), `validate_override` (only if any variant has `#[overrides]`), and `is_terminal`. Reject non-unit variants at macro expansion time with a clear error (same guard as `FlexibleEnum`). Add the `Transition` enum to `src/domain/transition.rs`.

#### Phase 2: Migrate Enums

Apply `#[derive(Fsm)]` to all 4 FSM enums. Add transition attributes. Keep the old `_transitions()` functions temporarily for comparison testing.

#### Phase 3: Update Handlers

Switch all 6 handler call sites from `validate_transition(from, to, role, &rules)` to `from.validate_transition(to, role)`. For the work handler's override path: `from.validate_override(to, role)`.

#### Phase 4: Cleanup

Remove `_transitions()` functions, `override_transitions()`, the `TransitionRule` struct, and the `validate_transition` free function. Update `fsm_correctness_tests.rs` - many tests become redundant since the macro guarantees the rules match the enum.

### All Four Enums with Proposed Attributes

#### HierarchyStatus

```rust
#[derive(Fsm)]
enum HierarchyStatus {
    #[transitions(Active(Coordinator), Abandoned(Coordinator))]
    Draft,
    #[transitions(Complete(Coordinator), Abandoned(Coordinator))]
    Active,
    Complete,
    Abandoned,
}
```

#### WorkStatus

```rust
#[derive(Fsm)]
enum WorkStatus {
    #[transitions(Ready(Coordinator), Abandoned(Coordinator))]
    Draft,
    #[transitions(InProgress(Coordinator), Blocked(Coordinator), Abandoned(Coordinator))]
    Ready,
    #[transitions(
        Blocked,
        InReview(Implementer),
        Abandoned(Coordinator),
    )]
    #[overrides(Ready(Coordinator), InReview(Coordinator))]
    InProgress,
    #[transitions(Ready(Coordinator), Abandoned(Coordinator))]
    Blocked,
    #[transitions(InProgress(Coordinator), Integrated(Integrator), Abandoned(Coordinator))]
    #[overrides(Ready(Coordinator))]
    InReview,
    #[transitions(Done(Coordinator, Integrator), Abandoned(Coordinator))]
    Integrated,
    Done,
    Abandoned,
}
```

#### BundleStatus

```rust
#[derive(Fsm)]
enum BundleStatus {
    #[transitions(
        Triaged(Coordinator),
        Rejected(Coordinator, Reviewer),
        Superseded(Coordinator),
    )]
    Proposed,
    #[transitions(
        Reviewed(Coordinator, Reviewer),
        Accepted(Coordinator),
        Rejected(Coordinator, Reviewer),
        Superseded(Coordinator),
    )]
    Triaged,
    #[transitions(
        Accepted(Coordinator),
        Rejected(Coordinator, Reviewer),
        Superseded(Coordinator),
    )]
    Reviewed,
    #[transitions(
        Integrating(Integrator),
        Rejected(Integrator),
        Superseded(Coordinator),
    )]
    Accepted,
    #[transitions(
        Merged(Integrator),
        Rejected(Integrator),
        Superseded(Coordinator),
    )]
    Integrating,
    Merged,
    Rejected,
    Superseded,
}
```

#### TickStatus

```rust
#[derive(Fsm)]
enum TickStatus {
    #[transitions(Sealing(Integrator), Failed(Integrator))]
    Open,
    #[transitions(Validating(Integrator), Failed(Integrator))]
    Sealing,
    #[transitions(Published(Integrator), Failed(Integrator))]
    Validating,
    Published,
    Failed,
}
```

## Alternatives Considered

### Alternative 1: Trait with Manual Match Arms

- **Description:** Define a `trait Fsm { fn validate_transition(...) }` and implement it manually on each enum with `match (self, target)` arms.
- **Pros:** No proc macro infrastructure. Co-located with the type.
- **Cons:** Still manual - the match arms ARE the boilerplate, just in a different shape. No compile-time guarantee that all non-terminal variants have transitions. Role logic mixed into match arms rather than declarative.
- **Why not chosen:** Same maintenance burden, different syntax.

### Alternative 2: One-Line Idempotency Fix Only

- **Description:** Just add `if current == target { return Ok(()) }` to `validate_transition` and leave the rule arrays as-is.
- **Pros:** Minimal change. Fixes the immediate death loop.
- **Cons:** Doesn't address the structural problem of disconnected rule arrays. Next time we add a state or transition, same drift risk.
- **Why not chosen:** Fixes the symptom, not the disease. Already applied as a bridge fix - this design replaces it properly.

### Alternative 3: External DSL / Config File

- **Description:** Define transitions in a YAML/JSON file, generate Rust code with a build script.
- **Pros:** Non-Rust-developers could edit transitions.
- **Cons:** Adds a build step, a DSL to learn, and disconnects the transitions from the Rust types. Loopr is maintained by Rust developers.
- **Why not chosen:** Over-engineered. The enum IS the right place for this.

## Technical Considerations

### Dependencies

No new external dependencies. The `loopr-derive` crate already depends on `syn`, `quote`, and `proc-macro2` for `FlexibleEnum`. The `Fsm` macro uses the same infrastructure.

### Role Path Resolution

The generated code references `crate::domain::role::Role`. This works because `loopr-derive` generates code that expands in the consuming crate's context. The macro doesn't need to know the Role variants at compile time - it emits them as identifiers and the Rust compiler resolves them. If a role name in an attribute doesn't match a `Role` variant, the generated code fails to compile with a clear error.

### Performance

Zero difference. The current code builds a `Vec<TransitionRule>` at runtime and does a linear scan. The generated code is a `match` expression - if anything, slightly faster (no allocation, no iteration). But for 5-20 rules this is irrelevant.

### Override Handling

`validate_override` is only generated if any variant has `#[overrides(...)]`. For enums without overrides (HierarchyStatus, BundleStatus, TickStatus), no `validate_override` method exists - trying to call it is a compile error. This is better than the current approach where `override_transitions()` is a free function that could be called on any enum.

**Semantic change from current code:** Today, `override_transitions()` is a standalone rule set - the handler uses either normal OR override rules, not both. Some edges are duplicated across both sets. The new design makes `validate_override` a superset: it tries normal transitions first, then override-only edges. This means `#[overrides]` only needs the additional edges, not duplicates of normal transitions. The behavioral difference is that an override call now accepts ALL normal transitions too, which is correct - an override should never be less permissive than normal.

### Testing Strategy

1. **Proc macro tests:** Snapshot tests in `loopr-derive` using `trybuild` or manual expansion checks. Verify generated code for a simple test enum.
2. **Integration tests:** For each migrated enum, verify that the generated `validate_transition` accepts exactly the same transitions as the old `_transitions()` function. Run both side-by-side before removing the old code.
3. **Idempotency tests:** Verify `state.validate_transition(state, role)` succeeds for all states and roles.
4. **Terminal tests:** Verify `is_terminal()` returns true only for variants without `#[transitions]`.
5. **Existing FSM correctness tests:** Update `fsm_correctness_tests.rs` - self-transition tests flip from "rejected" to "accepted", rule coverage tests verify attribute completeness.
6. **Compile-time tests:** Verify that a typo in a role name (e.g., `Coordnator`) fails to compile.

### Rollout Plan

Phases 1-4 are a single PR. The old functions and new attributes coexist during Phase 2-3 for comparison testing, then the old code is removed in Phase 4. No feature flags needed - this is a compile-time refactor with identical runtime behavior (plus idempotency).

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Proc macro bugs produce wrong validation | Low | High | Side-by-side comparison with old rules before removing them. Full FSM correctness test suite. |
| Role path `crate::domain::role::Role` breaks if module moves | Very Low | Low | Standard Rust path resolution. Any breakage is a compile error, not a runtime error. |
| Attribute syntax confusing for new contributors | Low | Low | The syntax reads naturally. Add a doc comment on the derive macro with examples. |
| Compile time increase from proc macro | Very Low | Low | `FlexibleEnum` already compiles with syn/quote. Adding one more derive is negligible. |
| Override semantics diverge from normal transitions | Low | Medium | `validate_override` calls `validate_transition` first, so normal transitions are always a subset. This is enforced structurally. |

## Open Questions

None remaining.

## Resolved Questions

- [x] **Should `validate_override` include normal transitions as a superset?** Yes. An override should never be less permissive than normal. The generated `validate_override` calls `validate_transition` first, then checks override-only edges. This means `#[overrides]` only lists additional edges, not duplicates.
- [x] **Should the macro validate role names at macro expansion time?** No. The macro emits role names as Rust identifiers. If a name is wrong (e.g., `Coordnator`), the Rust compiler catches it when resolving the generated `matches!(role, Role::Coordnator)` - with a clear "not found in `Role`" error. Simpler than teaching the macro about Role, and the error message is good enough.
- [x] **Does idempotency bypass role authorization?** Yes. The `if self == target` check occurs *before* evaluating the match arms and role constraints. This means an agent could technically request an idempotent transition (e.g., `Draft -> Draft`) even if they don't have authorization for the `Draft` state. This is an accepted design choice: a pure idempotency model where requesting the current state is universally harmless. Because `Transition::Unchanged` prevents DB writes and events, unauthorized idempotent calls produce no system side-effects.

## References

- `loopr-derive/src/lib.rs` - existing `FlexibleEnum` proc macro
- `src/domain/transition.rs` - current `validate_transition` and `TransitionRule`
- `src/domain/plan.rs:43-66` - `hierarchy_transitions()`
- `src/domain/work.rs:31-160` - `work_transitions()` and `override_transitions()`
- `src/domain/bundle.rs:31-141` - `bundle_transitions()`
- `src/domain/tick.rs:35-71` - `tick_transitions()`
- `src/daemon/handlers/*.rs` - 6 call sites
- `src/fsm_correctness_tests.rs` - existing FSM test suite
- `docs/design/2026-04-01-noop-dirty-worktree-guard.md` - the bug that exposed the idempotency gap
