# Design Document: Convert AgenticLlm to Generic

**Author:** Scott A. Idler
**Date:** 2026-04-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

`DelegateTool` holds `Arc<dyn AgenticLlm>` and `ToolExecutor::chat_with_delegation`
accepts `Arc<dyn AgenticLlm>`. Converting both to `<L: AgenticLlm + Send + Sync +
'static>` removes the `dyn`, enables `#[async_trait]` removal from `AgenticLlm` and all
its impls, and is transparent to `ToolExecutor`'s `HashMap<String, Box<dyn Tool>>`
because `DelegateTool<L>` is erased to `Box<dyn Tool>` at storage time.

## Problem Statement

### Background

`docs/design/2026-04-05-post-async-migration-cleanup.md` Phase 2 blocked on four async
traits used as `dyn`. `AgenticLlm` is the second-smallest blast radius: two struct/field
sites plus two internal function signatures.

### Problem

`AgenticLlm` has async methods (`complete`, `complete_streaming`). Stored as
`Arc<dyn AgenticLlm>` in `DelegateTool` and passed as `Arc<dyn AgenticLlm>` into
`ToolExecutor::chat_with_delegation`, this prevents native async fn in trait use and
keeps `#[async_trait]` on the trait and all mock impls.

### Goals

- Remove `Arc<dyn AgenticLlm>` from `DelegateTool` and `chat_with_delegation`
- Replace with `<L: AgenticLlm + Send + Sync + 'static>` generic
- Remove `&dyn AgenticLlm` from internal `run_tool_loop` and `auto_compact` signatures
- Remove `#[async_trait]` from `AgenticLlm` trait and all its impls
- `otto ci` passes

### Non-Goals

- Converting `Box<dyn Tool>` in `ToolExecutor` (stays `dyn`)
- Any other `dyn` traits

## Proposed Solution

### Implementation Plan

**`src/tools/builtin/delegate.rs`:**

```rust
// Before
pub struct DelegateTool {
    llm: Arc<dyn AgenticLlm>,
    executor: Arc<ToolExecutor>,
}
impl DelegateTool {
    pub fn new(llm: Arc<dyn AgenticLlm>, executor: Arc<ToolExecutor>) -> Self { ... }
}

// After
pub struct DelegateTool<L: AgenticLlm + Send + Sync + 'static> {
    llm: Arc<L>,
    executor: Arc<ToolExecutor>,
}
impl<L: AgenticLlm + Send + Sync + 'static> DelegateTool<L> {
    pub fn new(llm: Arc<L>, executor: Arc<ToolExecutor>) -> Self { ... }
}
```

`DelegateTool<L>` still implements `Tool` with `#[async_trait]` (Tool stays `dyn`).
The concrete `L` is erased when `Box::new(delegate)` is stored in `ToolExecutor::tools`.

**`src/tools/executor.rs`:**

```rust
// Before
pub fn chat_with_delegation(configured: &[ToolEntry], llm: Arc<dyn AgenticLlm>) -> Self

// After
pub fn chat_with_delegation<L: AgenticLlm + Send + Sync + 'static>(
    configured: &[ToolEntry],
    llm: Arc<L>,
) -> Self
```

**`src/tools/agentic_loop.rs`:**

```rust
// Before
async fn auto_compact(llm: &dyn AgenticLlm, ...) { ... }
pub async fn run_tool_loop(llm: &dyn AgenticLlm, ...) -> Result<...>

// After
async fn auto_compact<L: AgenticLlm>(llm: &L, ...) { ... }
pub async fn run_tool_loop<L: AgenticLlm>(llm: &L, ...) -> Result<...>
```

After all changes, remove `#[async_trait]` from `AgenticLlm` trait definition and all
`impl AgenticLlm for ...` blocks. Remove `use async_trait::async_trait` from affected files.

**Commit:**
```
refactor(tools): convert Arc<dyn AgenticLlm> to generic L: AgenticLlm
```

## Alternatives Considered

### Alternative: Keep Arc<dyn AgenticLlm>

- **Pros:** No change
- **Cons:** async_trait on tool loop layer; violates rust.md DI rule
- **Why not chosen:** Generic erases cleanly into Box<dyn Tool>; no monomorphization cost
  at the storage level

## Technical Considerations

### Callsites for run_tool_loop

Verify before implementing: every caller of `run_tool_loop` passes a single concrete
`AgenticLlm` type. If any caller passes different concrete types in branches, generic
dispatch may be insufficient and `&dyn AgenticLlm` must stay for that function.

### DelegateTool<L> object-safety for Tool

`DelegateTool<L>` implements `Tool`. The `Tool` trait's `execute` method does not
reference `L`. `Box<dyn Tool>` is sound because the vtable for `Tool` is built over
`DelegateTool<L>`'s concrete impl, with `L` erased. Rust allows this.

### Testing Strategy

- `otto ci` after the change
- Test mocks in `src/tools/agentic_loop.rs` tests (`MockAgenticLlm`, `StreamingMockLlm`)
  lose `#[async_trait]` and are passed as concrete types, not `Arc<dyn>`

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| run_tool_loop caller passes heterogeneous types | Low | Med | Grep all callers before starting |
| DelegateTool<L>: Tool bounds don't include L so erasing fails | Low | High | Verify with cargo check after first change |

## Open Questions

- [ ] Confirm all `run_tool_loop` callers pass a single concrete `AgenticLlm` type.

## References

- `src/tools/builtin/delegate.rs` - `DelegateTool`
- `src/tools/executor.rs` - `ToolExecutor::chat_with_delegation`
- `src/tools/agentic_loop.rs` - `run_tool_loop`, `auto_compact`
- `docs/design/2026-04-05-post-async-migration-cleanup.md` - parent doc (Phase 2)
