# Design Document: Convert LlmClient to Generic

**Author:** Scott A. Idler
**Date:** 2026-04-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

`CoordinatorAgent`, `ImplementerAgent`, `ReviewerAgent`, and `ResearcherAgent` each hold
`Box<dyn LlmClient>`. Converting all four structs to `<L: LlmClient>` removes the `dyn`,
enables `#[async_trait]` removal from `LlmClient` and `AgentLlmClient`, and is
mechanically guided by the compiler. The only non-trivial callsite is
`src/agents/executor/lifecycle.rs`, which constructs all four agent types.

## Problem Statement

### Background

`docs/design/2026-04-05-post-async-migration-cleanup.md` Phase 2 blocked on four async
traits used as `dyn`. `LlmClient` (defined in `src/agents/implementer.rs`) is the
largest blast radius of the three feasible refactors: four structs, their constructors,
and one lifecycle module that instantiates them all.

### Problem

`LlmClient` has async methods (`call`, `call_with_history`). Stored as `Box<dyn
LlmClient>` in four agent structs, this prevents native async fn in trait use and keeps
`#[async_trait]` on the largest cluster of annotations in the codebase.

### Goals

- Remove `Box<dyn LlmClient>` from all four agent structs
- Replace with `<L: LlmClient>` generic parameter on each struct
- Update constructors and `lifecycle.rs`
- Remove `#[async_trait]` from `LlmClient` trait and `AgentLlmClient` impl
- `otto ci` passes

### Non-Goals

- Converting any other `dyn` trait
- Removing `async_trait` crate (Tool impls still use it)
- Changing `LlmClient` method signatures

## Proposed Solution

### Overview

All four agent structs get a `<L: LlmClient>` generic. The production `AgentLlmClient`
type from `src/agents/llm_client.rs` is the concrete type at runtime. Test mocks
(`MockLlm`, `FailingLlm`, etc.) become concrete types instead of `Box<dyn LlmClient>`.

### Implementation Plan

**`src/agents/implementer.rs`:**

```rust
// Before
pub struct ImplementerAgent {
    llm: Box<dyn LlmClient>,
    ...
}
impl ImplementerAgent {
    pub fn new(ctx: AgentContext, llm: Box<dyn LlmClient>, ...) -> Self { ... }
}

// After
pub struct ImplementerAgent<L: LlmClient> {
    llm: L,
    ...
}
impl<L: LlmClient> ImplementerAgent<L> {
    pub fn new(ctx: AgentContext, llm: L, ...) -> Self { ... }
}
```

Same pattern for `CoordinatorAgent<L>`, `ReviewerAgent<L>`, `ResearcherAgent<L>`.

**`src/agents/executor/lifecycle.rs`:**

This file matches on `AgentKind` and constructs the appropriate agent. It becomes generic
over `L: LlmClient`:

```rust
pub async fn run_agent<L: LlmClient + Clone + Send + Sync + 'static>(
    kind: AgentKind,
    ctx: AgentContext,
    llm: L,
    ...
) -> Result<()> {
    match kind {
        AgentKind::Coordinator => CoordinatorAgent::new(ctx, llm, ...).run().await,
        AgentKind::Implementer => ImplementerAgent::new(ctx, llm, ...).run().await,
        ...
    }
}
```

The daemon that calls `run_agent` passes `AgentLlmClient` (concrete production type).

**`src/agents/llm_client.rs`:**

After the struct changes, remove `#[async_trait]` from `LlmClient` trait definition
and `impl LlmClient for AgentLlmClient`.

**Test files:**

Test code that does `let llm: Box<dyn LlmClient> = Box::new(MockLlm {...})` becomes
`let llm = MockLlm {...}`. The type annotation changes from `Box<dyn LlmClient>` to the
concrete mock type or is elided. Constructor calls change from
`ImplementerAgent::new(ctx, Box::new(mock))` to `ImplementerAgent::new(ctx, mock)`.

**Commit:**
```
refactor(agents): convert Box<dyn LlmClient> to generic L: LlmClient
```

### Order of edits

1. Add `<L: LlmClient>` to each agent struct and constructor (one file at a time)
2. Update `lifecycle.rs` to be generic over `L`
3. Update daemon callsite(s) to pass `AgentLlmClient` concretely
4. Update test mock callsites (compiler guides)
5. Remove `#[async_trait]` from `LlmClient` trait and `AgentLlmClient` impl
6. `otto ci`

## Alternatives Considered

### Alternative: Keep Box<dyn LlmClient>

- **Pros:** No change; tests stay with `Box<dyn>` pattern
- **Cons:** async_trait on agent layer forever; `rust.md` DI violation; no RPITIT opt
- **Why not chosen:** Compiler fully guides the migration; test code simplifies

### Alternative: Use Arc<L> instead of L directly

- **Description:** Store `Arc<L>` to allow cheap cloning
- **Pros:** Cheaper to clone agents
- **Cons:** Adds indirection; agents aren't currently cloned; premature optimization
- **Why not chosen:** Keep it simple; `L` directly matches the ownership model already

## Technical Considerations

### LlmClient + Clone requirement

The `run_agent` dispatch may need `L: Clone` if the same `llm` is passed to different
agent variants (e.g. in a retry loop). Verify whether `AgentLlmClient` implements
`Clone`. If not, `Arc<AgentLlmClient>` is the right wrapper at the call site.

### ResearcherAgent

`ResearcherAgent` may have a different `LlmClient` usage pattern (check if it holds
its own LLM vs. using a shared one). The grep showed it in the construction list but
verify the struct definition before assuming it mirrors the others.

### Testing Strategy

- Convert one agent at a time, `cargo check` between each
- `otto ci` after all four structs + lifecycle + async_trait removal

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `AgentLlmClient` does not implement `Clone` | Med | Med | Wrap in `Arc<AgentLlmClient>` at daemon callsite |
| lifecycle.rs needs a trait bound not on `LlmClient` | Low | Low | Compiler reveals it; add the bound |
| Test mocks need `Send + Sync + 'static` bounds they lack | Low | Low | Add derives or `unsafe impl`; mock types are simple structs |
| ResearcherAgent doesn't follow the same pattern | Low | Med | Read the struct before assuming |

## Open Questions

- [ ] Does `AgentLlmClient` implement `Clone`? If not, use `Arc<AgentLlmClient>`.
- [ ] Does `ResearcherAgent` hold `Box<dyn LlmClient>` or a different LLM abstraction?
- [ ] Are there any other construction sites for the four agent types outside
  `lifecycle.rs` (e.g. in integration tests)?

## References

- `src/agents/implementer.rs` - `LlmClient` trait definition + `ImplementerAgent` struct
- `src/agents/coordinator.rs` - `CoordinatorAgent` struct
- `src/agents/reviewer.rs` - `ReviewerAgent` struct
- `src/agents/researcher.rs` - `ResearcherAgent` struct
- `src/agents/executor/lifecycle.rs` - agent construction dispatch
- `src/agents/llm_client.rs` - `AgentLlmClient` concrete impl
- `docs/design/2026-04-05-post-async-migration-cleanup.md` - parent doc (Phase 2)
