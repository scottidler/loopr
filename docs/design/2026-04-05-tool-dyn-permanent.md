# Design Document: Tool Trait - dyn Stays

**Author:** Scott A. Idler
**Date:** 2026-04-05
**Status:** Superseded by 2026-04-05-async-trait-eradication.md
**Review Passes Completed:** 5/5

## Summary

`ToolExecutor` holds `HashMap<String, Box<dyn Tool>>` - a heterogeneous collection of
15+ distinct tool types. This is the textbook correct use of `dyn` dispatch. Converting
to generics would require enum dispatch or complete redesign. `#[async_trait]` stays on
`Tool` and all its implementations permanently unless that redesign is undertaken.

## Problem Statement

### Background

`docs/design/2026-04-05-post-async-migration-cleanup.md` Phase 2 identified four async
traits used as `dyn`. Three are amenable to generic conversion. `Tool` is not.

### Problem

`Tool` is stored as `HashMap<String, Box<dyn Tool>>` in `ToolExecutor`. The map holds
instances of `EditTool`, `ReadTool`, `ShellTool`, `FetchTool`, `DelegateTool<L>`,
`ConfiguredTool`, and ~9 other types — all different, all mixed at runtime based on
configuration and tool registration. This is exactly what `dyn` exists for.

### Decision

**`dyn Tool` is correct here. Do not convert.**

Heterogeneous collections of concrete types where the set is determined at runtime
(or by user config) require `dyn` dispatch. The alternatives are worse:

1. **Enum dispatch** - `enum AnyTool { Edit(EditTool), Read(ReadTool), ... }` requires
   adding a variant for every new tool, hard-codes the tool set, and couples the dispatch
   mechanism to individual tool implementations.

2. **Type-erased arc with downcasting** - `Arc<dyn Any>` is unsafe and defeats the
   purpose of a trait boundary.

3. **Monomorphized ToolExecutor** - Impossible; the set of tools is dynamic (configured
   tools are resolved at runtime from `ToolEntry` config).

### Consequences

- `#[async_trait]` stays on `Tool` trait and all ~14-15 builtin `impl Tool` blocks
- `async-trait` crate cannot be fully removed even after all other conversions complete
- The count of `#[async_trait]` annotations after the three other conversions is ~14,
  down from ~35

### Non-Goals

- Enum dispatch redesign for tools (out of scope, not worth the complexity)
- Any change to `Tool` trait, `ToolExecutor`, or the builtin tool implementations

## Why This Document Exists

To explicitly record the decision so it is not revisited without cause. Future readers
encountering `#[async_trait]` on `Tool` impls should not assume it was overlooked.

## References

- `src/tools/executor.rs` - `ToolExecutor` with `HashMap<String, Box<dyn Tool>>`
- `src/tools/traits.rs` - `Tool` trait definition
- `src/tools/builtin/*.rs` - 14 builtin tool impls, all `#[async_trait]`
- `docs/design/2026-04-05-post-async-migration-cleanup.md` - parent doc (Phase 2)
