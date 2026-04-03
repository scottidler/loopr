# Design Document: Encapsulate FSM Status Fields

**Author:** Scott A. Idler
**Date:** 2026-04-02
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Make the `status` field private on all FSM-governed domain structs (Work, Bundle, Tick, Plan/Spec/Phase, Lock). Expose status reads via `pub fn status(&self)` and writes via validated `transition()` / explicit `force_status()` methods. This turns every current direct `.status = X` bypass into a compiler error, forcing each mutation site to declare its intent: validated transition, recovery override, or test fixture.

## Problem Statement

### Background

Loopr's FSM layer (`#[derive(Fsm)]`) generates correct `validate_transition()` and `validate_override()` methods. The handler layer (daemon/handlers/) calls these before applying state changes. The FSM transition tests are comprehensive. Despite this, the system repeatedly wedges into unrecoverable states.

### Problem

The FSM is a gatekeeper that half the codebase walks around. There are **79 direct `.status = X` mutations** in non-test production code. Of these:

- **6** are in handlers, after `validate_transition()` - the correct path
- **4** are crash recovery in `context.rs` - intentional bypasses
- **6** are bootstrap/seed paths in coordinator and manifest handlers - intentional bypasses
- **3** are integrator handler tick mutations (`handlers/integrator.rs`) - should use validated transition
- **1** is coordinator agent code (`coordinator.rs:752`) mutating `phase.status` directly
- **~14** are daemon/supervisor/TUI code mutating `session.status` directly, bypassing `AgentSession::transition_to()` which already exists
- **2** are Lock domain methods (`release()`, `expire()`)

Note: `AgentSession` already has a validated `transition_to()` method but 14 sites bypass it by setting `.status` directly. This is the exact same class of bug - making the field private catches these too.

The unvalidated bypasses are the dangerous ones. An agent directly mutating `phase.status = Complete` or a handler setting `tick.status = Validating` without `validate_transition()` has no validation, no event broadcast, and no audit trail. Similarly, the 14 sites that bypass `AgentSession::transition_to()` can put sessions into invalid states. When these mutations fail halfway through a multi-step operation, the system fractures.

The current `pub status` field makes it impossible to distinguish intentional bypasses from accidental ones. The compiler cannot help. Every new line of agent code can introduce a new bypass without anyone noticing.

### Goals

- Every status mutation in non-test code must go through one of two explicit paths: `transition()` (validated) or `force_status()` (intentional bypass)
- The compiler rejects direct `.status = X` on FSM-governed structs
- `force_status()` calls are greppable - a future audit of all bypass sites is a single search
- Serde deserialization continues to work (records loaded from TaskStore/JSONL)
- Reading status remains zero-cost (`record.status()` not `record.status`)

### Non-Goals

- This does NOT fix the two-layer divergence (git state vs DB state) - that's Option B (reconciliation sweep)
- This does NOT add new transition rules or preconditions
- This does NOT change the `#[derive(Fsm)]` proc macro
- This does NOT encapsulate non-FSM fields (only `status`)
- This does NOT change test code patterns - tests can use `force_status()` freely

## Proposed Solution

### Overview

For each FSM-governed domain struct, change `pub status: XStatus` to a private field with accessor methods. The `#[derive(Fsm)]` macro is not changed - it already generates methods on the status enum. The encapsulation lives on the **struct**, not the enum.

### API Design

Each FSM-governed struct gains three methods:

```rust
impl Work {
    /// Read current status.
    pub fn status(&self) -> WorkStatus {
        self.status
    }

    /// Validated FSM transition. Returns Err if invalid.
    pub fn transition(
        &mut self,
        target: WorkStatus,
        role: Role,
    ) -> crate::error::Result<Transition> {
        let result = self.status.validate_transition(target, role)?;
        if result == Transition::Changed {
            self.status = target;
            self.updated_at = crate::id::now_millis();
        }
        Ok(result)
    }

    /// Validated FSM override transition (Work only - has override edges).
    pub fn transition_override(
        &mut self,
        target: WorkStatus,
        role: Role,
    ) -> crate::error::Result<Transition> {
        let result = self.status.validate_override(target, role)?;
        if result == Transition::Changed {
            self.status = target;
            self.updated_at = crate::id::now_millis();
        }
        Ok(result)
    }

    /// Bypass FSM validation. For recovery, bootstrap, and test fixtures ONLY.
    /// Every call site is a potential wedge source - grep for audit.
    pub fn force_status(&mut self, target: WorkStatus) {
        self.status = target;
        self.updated_at = crate::id::now_millis();
    }
}
```

The pattern is identical for Bundle, Tick, Plan (HierarchyStatus), Spec, Phase, and Lock. Only Work has `transition_override` (it's the only type with `#[overrides]`).

`AgentSession` already has this pattern (`transition_to()` with `can_transition_to()` validation) but its `status` field is still `pub`, so callers bypass it. Making the field private completes the encapsulation that was already half-done.

### Serde Compatibility

Serde can deserialize private fields when the struct uses `#[derive(Deserialize)]` - it uses the field name, not visibility. No change needed for deserialization.

For serialization, same thing - `#[derive(Serialize)]` accesses private fields directly.

The `indexed_fields()` method in the `Record` impl already uses `self.status.to_string()` - just change to `self.status().to_string()` (or keep accessing the private field since it's in the same module).

### Data Model

No changes to stored data. The JSONL/SQLite format is unchanged. This is purely a code-level encapsulation.

### Implementation Plan

The work is organized **per-struct**, not per-concern. Each struct is a self-contained commit: make the field private, add methods, fix all callers (reads, writes, tests), run `otto ci`. This avoids a broken intermediate state where the field is private but callers aren't updated.

#### Struct order (fewest callers first)

1. **Lock** - 2 production mutations, ~1 test. Proof of concept. `release()`/`expire()` become wrappers around private field.
2. **Tick** - 3 handler mutations, ~5 test. Small scope, high value (tick wedges are common).
3. **Plan/Spec/Phase** (HierarchyStatus) - 6 bootstrap mutations, ~20 test. All three share the same status type, so batch them.
4. **Bundle** - 3 handler mutations, ~15 test. Moderate scope.
5. **Work** - 2 handler mutations + 1 agent bypass, ~30 test. Highest-value struct (most wedge bugs).
6. **AgentSession** - ~14 daemon/supervisor/TUI bypasses, ~20 test. Already has `transition_to()` - just make the field private.

#### What changes per struct

For each struct:
1. Change `pub status: XStatus` to `status: XStatus`
2. Add `status()`, `transition()`, `force_status()` methods
3. Fix all `.status ==` reads to `.status() ==`
4. Categorize each `.status = X` write:

| Category | Replace with | Example |
|----------|-------------|---------|
| Handler after `validate_transition()` | `record.transition(target, role)?` - collapse validate+mutate into one call | `handlers/work.rs` |
| Crash recovery | `record.force_status(target)` | `context.rs` |
| Bootstrap/seed | `record.force_status(target)` | `handlers/coordinator.rs`, `manifest.rs` |
| Should-be-validated | `record.transition(target, role)?` | `handlers/integrator.rs` tick mutations |
| Test fixture | `record.force_status(target)` | all test files |

5. Run `otto ci`

#### Before/after: handler example

**Before** (handlers/work.rs - two-step validate then mutate):
```rust
let from = wi.status;
let result = if is_override {
    from.validate_override(target_status, role)
} else {
    from.validate_transition(target_status, role)
};
match result {
    Err(e) => return error,
    Ok(Transition::Unchanged) => return success_null,
    Ok(Transition::Changed) => {}
}
// ... precondition checks ...
wi.status = target_status;
wi.updated_at = crate::id::now_millis();
```

**After** (single validated call):
```rust
let from = wi.status();
// ... precondition checks ...
let result = if is_override {
    wi.transition_override(target_status, role)
} else {
    wi.transition(target_status, role)
};
match result {
    Err(e) => return error,
    Ok(Transition::Unchanged) => return success_null,
    Ok(Transition::Changed) => {}
}
// status and updated_at already set by transition()
```

**Capturing `from`:** Capture the pre-transition state before calling `transition()` - needed for event broadcasts and error messages:
```rust
let from = wi.status();
// precondition checks...
let result = wi.transition(target_status, role);
// from is still valid here for DaemonEvent::transition_completed()
```

**`matches!()` patterns:** ~20 sites use `matches!(record.status, Pattern)` in addition to the `==` comparisons. These become `matches!(record.status(), Pattern)` - same mechanical change.

**`indexed_fields()` access:** The `Record` impl accesses `self.status` directly. This continues to work because Rust visibility is module-scoped: the `impl` block is in the same file as the struct definition, so private fields are accessible.

## Alternatives Considered

### Alternative 1: Newtype wrapper around status

```rust
pub struct ValidatedStatus<S>(S);
```

- **Description:** Wrap all status fields in a newtype that only allows mutation through methods.
- **Pros:** Could be generic across all status types.
- **Cons:** Adds a layer of indirection. Complicates serde. Complicates pattern matching on status values. More cognitive overhead than it saves.
- **Why not chosen:** The methods-on-struct approach is simpler and more idiomatic Rust.

### Alternative 2: Lint rule (clippy) instead of encapsulation

- **Description:** Write a custom clippy lint that flags direct `.status =` on FSM structs.
- **Pros:** No code changes to domain structs.
- **Cons:** Warnings, not errors. Easily ignored. Custom clippy lints are complex to write and maintain. Doesn't run at compile time in the same way.
- **Why not chosen:** Compiler errors >> warnings. The whole point is making bypasses impossible to introduce accidentally.

### Alternative 3: Generate the methods in `#[derive(Fsm)]`

- **Description:** Extend the proc macro to generate `transition()` and `force_status()` on the containing struct.
- **Pros:** Less boilerplate per struct.
- **Cons:** The proc macro derives on the *enum*, not the struct. It doesn't know the struct name, field name, or that `updated_at` exists. Would require a second derive macro on each struct, which is more complexity than writing the methods directly.
- **Why not chosen:** The methods are ~15 lines per struct. 9 structs = ~135 lines total. Not worth a proc macro.

### Alternative 4: Do nothing, rely on code review

- **Description:** Just be more careful about direct mutations.
- **Pros:** Zero effort.
- **Cons:** Already proven to fail. The last two weeks of bug fixes are evidence.
- **Why not chosen:** We've tried this. It doesn't work.

## Technical Considerations

### Dependencies

None. This is a refactor of existing code with no new crates.

### Performance

Zero impact. `status()` returns a `Copy` type. The compiler will inline it.

### Testing Strategy

- `otto ci` must pass after each phase
- The transition tests in `src/tests/fsm/` continue to work unchanged (they test the enum methods, not struct methods)
- New tests: one per struct verifying that `transition()` rejects invalid transitions and `force_status()` always succeeds
- Grep audit: `grep -rn 'force_status' src/ --include='*.rs' | grep -v tests` should produce a stable, auditable list

### Rollout Plan

One commit per struct, ordered by the struct list in the Implementation Plan. Each commit compiles and passes `otto ci` independently. No feature flags.

### Verification

After all structs are encapsulated, these commands confirm completeness:

```bash
# No direct .status assignments remain outside domain modules
grep -rn '\.status\s*=' src/ --include='*.rs' | grep -v '\.status\s*==' | grep -v 'self\.status' | grep -v 'force_status' | grep -v 'tests'
# Should return zero results

# All force_status() calls in production code (the audit list)
grep -rn 'force_status' src/ --include='*.rs' | grep -v tests | grep -v '#\[cfg(test)\]'
# Should be ~10-15 sites: crash recovery + bootstrap only
```

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Serde deserialization breaks with private field | Low | High | Serde handles private fields in derived impls. Verify with existing roundtrip tests before proceeding. |
| Agent-side mutations that should be IPC get `force_status()` instead | Medium | Medium | Each site reviewed individually. Comment every `force_status()` with why it bypasses. |
| `force_status()` becomes the new `pub status` - used everywhere | Medium | Medium | Code review norm: `force_status()` in non-test, non-recovery code requires justification. Grep audit in CI. |
| Large mechanical diff obscures logic changes | Low | Low | Separate commits per struct. Read changes are purely mechanical. |

## Open Questions

- [ ] Should `force_status()` log a warning when called outside `#[cfg(test)]`? This would make runtime bypass usage visible in daemon logs. Downside: crash recovery calls `force_status()` on every restart, which is noisy.
- [ ] Should `Decision` and `Proposal` (which don't use `#[derive(Fsm)]`) also get encapsulated for consistency, or leave them as-is since they have no FSM validation to enforce?

## References

- `docs/design/2026-04-01-derive-fsm.md` - the `#[derive(Fsm)]` design and implementation
- `docs/design/2026-04-01-silent-error-audit.md` - the audit that found the 22 silent-error sites
- Prior conversation analysis: FSM two-layer divergence assessment (2026-04-02)
