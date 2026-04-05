# Design Document: Eradicate async_trait and Rectify Guideline Violations

**Author:** Scott A. Idler
**Date:** 2026-04-05
**Status:** Implemented
**Review Passes Completed:** 5/5
**Supersedes:** `2026-04-05-tool-dyn-permanent.md` (decision to keep async_trait on Tool)

## Summary

Remove every `#[async_trait]` annotation and the `async-trait` crate dependency from the codebase. The Agent trait (no dyn dispatch) converts to native `async fn`. The Tool trait (requires `Box<dyn Tool>` for heterogeneous storage) converts to a manual `Pin<Box<dyn Future>>` return - the same desugaring async_trait performs, but owned explicitly with no macro dependency. Additionally, remove the orphaned `decision` and `proposal` domain modules that carry `#[allow(dead_code)]` in violation of project rules. Update `CLAUDE.md` and `rust.md` to ban `async-trait` going forward.

## Problem Statement

### Background

The v0.1.76-v0.1.79 refactoring arc converted `LlmClient`, `AgenticLlm`, and `HttpClient` from `dyn` trait objects to generics and removed their `#[async_trait]` annotations. Those traits now use the `fn -> impl Future + Send + 'a` form. The `async-trait` crate remains in `Cargo.toml` solely for the `Agent` trait (6 impls) and the `Tool` trait (17 impls).

A prior design doc (`tool-dyn-permanent.md`) declared that `#[async_trait]` would stay on `Tool` permanently because `Box<dyn Tool>` requires object safety. That document was written before we considered the manual desugaring approach. It is now superseded.

### Problem

1. **23 residual `#[async_trait]` annotations** across `Agent` (6) and `Tool` (17) traits - the crate is a bridge dependency from the pre-generic era with no remaining justification
2. **`async-trait` crate in Cargo.toml** - dead weight; adds proc-macro compile time for something Edition 2024 handles natively (Agent) or can be expressed in 1 line of manual desugaring (Tool)
3. **`#[allow(dead_code)]` on `domain::decision` and `domain::proposal`** - modules registered in `Stores` but never created/queried in any production code path; violates the project's `#[deny(dead_code)]` stance
4. **`tool-dyn-permanent.md` approved a permanent dependency** that this doc now eliminates

### Goals

- Zero `#[async_trait]` annotations in the codebase
- `async-trait` removed from `Cargo.toml`
- Zero `#[allow(dead_code)]` in `src/domain.rs`
- `CLAUDE.md` and `rust.md` updated to ban `async-trait` with the rationale
- `otto ci` green after each phase

### Non-Goals

- Changing `Box<dyn Tool>` to an enum or generic - dyn dispatch is correct for heterogeneous tool storage
- Removing `#![allow(clippy::manual_async_fn)]` from `lib.rs` - still needed for `LlmClient`/`AgenticLlm`/`HttpClient` traits that use `fn -> impl Future + Send + 'a`
- Modifying any production behavior
- Fixing `_git_guard` in `src/agents/integrator.rs:561` - this is a legitimate RAII guard pattern where the variable must live for its scope; the underscore prefix prevents an unused-variable warning while keeping the lock held. RAII guards are an accepted exception to the no-underscore rule.
- Fixing `__result` in `try_handler!` / `try_async_handler!` macros in `src/daemon/handlers.rs` - double underscore is standard macro hygiene to avoid name collisions with user code

## Proposed Solution

### Overview

Four phases in dependency order. Each phase ends with `otto check` green before proceeding.

```
Phase 1: Agent trait - native async fn         (6 files, ~12 line deletions)
Phase 2: Tool trait - manual Pin<Box<Future>>  (18 files, mechanical transform)
Phase 3: Remove async-trait crate + update rules (Cargo.toml + 2 doc files)
Phase 4: Remove dead domain modules            (3 files)
```

### Phase 1: Agent trait - native async fn

The `Agent` trait is never used as `dyn Agent`. The daemon dispatches agents via a `match` on `AgentKind` in `src/agents/executor/lifecycle.rs:320-371`, instantiating each concrete type independently. No `Box<dyn Agent>` or `Arc<dyn Agent>` exists anywhere in the codebase.

**Trait change:**

```rust
// Before
#[async_trait]
pub trait Agent: Send {
    async fn run(&mut self) -> Result<()>;
    fn agent_type(&self) -> AgentKind;
}

// After
pub trait Agent: Send {
    fn run(&mut self) -> impl Future<Output = Result<()>> + Send + '_;
    fn agent_type(&self) -> AgentKind;
}
```

Using `fn -> impl Future + Send + '_` (RPITIT) rather than bare `async fn` because bare `async fn` in traits does not guarantee `Send` on the returned future. The explicit form matches the pattern already established for `LlmClient`, `AgenticLlm`, and `HttpClient`.

**Important:** Each `impl Agent` block keeps its `async fn run` body unchanged. The compiler desugars `async fn run(&mut self) -> Result<()>` in the impl into a future that satisfies the trait's `impl Future<Output = Result<()>> + Send + '_` bound - no manual wrapping required. Only the `#[async_trait]` attribute and its `use` import are deleted from each impl file.

**Files changed (6):**

| File | Change |
|------|--------|
| `src/agents.rs` | Remove `use async_trait`, remove `#[async_trait]` from trait def |
| `src/agents/coordinator.rs` | Remove `use async_trait`, remove `#[async_trait]` from impl |
| `src/agents/implementer.rs` | Remove `use async_trait`, remove `#[async_trait]` from impl |
| `src/agents/integrator.rs` | Remove `use async_trait`, remove `#[async_trait]` from impl |
| `src/agents/researcher.rs` | Remove `use async_trait`, remove `#[async_trait]` from impl |
| `src/agents/reviewer.rs` | Remove `use async_trait`, remove `#[async_trait]` from impl |

**Validation:** `otto check` - compiler will catch any missed `dyn Agent` usage.

### Phase 2: Tool trait - manual Pin<Box<dyn Future>>

`Tool` IS used as `Box<dyn Tool>` in `ToolExecutor::tools: HashMap<String, Box<dyn Tool>>`. This is correct - heterogeneous tool registration from config requires dynamic dispatch. But `#[async_trait]` is not the only way to achieve this.

`#[async_trait]` is a proc macro that mechanically desugars:
```rust
async fn execute(&self, ...) -> ToolResult
```
into:
```rust
fn execute<'life0, 'life1, 'async_trait>(
    &'life0 self, ...
) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'async_trait>>
where 'life0: 'async_trait, 'life1: 'async_trait
```

We write this ourselves, simplified to one explicit lifetime:

**Trait change:**

```rust
// Before
use async_trait::async_trait;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult;
}

// After
use std::future::Future;
use std::pin::Pin;

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    fn execute<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a super::context::ToolContext,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;
}
```

**Impl change pattern (all 17 impls):**

```rust
// Before
#[async_trait]
impl Tool for ReadTool {
    // ...
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        // body
    }
}

// After
impl Tool for ReadTool {
    // ...
    fn execute<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            // body (unchanged)
        })
    }
}
```

The transformation is mechanical: wrap the body in `Box::pin(async move { ... })`, change the signature, remove the attribute and import.

**Files changed (18):**

| File | Change |
|------|--------|
| `src/tools/traits.rs` | Trait definition: `async fn` to `Pin<Box<>>` |
| `src/tools/configured.rs` | `ConfiguredTool` impl |
| `src/tools/builtin/read.rs` | `ReadTool` impl |
| `src/tools/builtin/write.rs` | `WriteTool` impl |
| `src/tools/builtin/edit.rs` | `EditTool` impl |
| `src/tools/builtin/list.rs` | `ListTool` impl |
| `src/tools/builtin/tree.rs` | `TreeTool` impl |
| `src/tools/builtin/glob.rs` | `GlobTool` impl |
| `src/tools/builtin/grep.rs` | `GrepTool` impl |
| `src/tools/builtin/find.rs` | `FindTool` impl |
| `src/tools/builtin/shell.rs` | `ShellTool` impl |
| `src/tools/builtin/slash.rs` | `SlashTool` impl |
| `src/tools/builtin/fetch.rs` | `FetchTool` impl |
| `src/tools/builtin/search.rs` | `SearchTool` impl |
| `src/tools/builtin/todo.rs` | `TodoTool` impl |
| `src/tools/builtin/plan.rs` | `PlanTool` impl |
| `src/tools/builtin/delegate.rs` | `DelegateTool<L>` impl |

**Notes on specific impls:**

- `DelegateTool<L>` already carries `L: AgenticLlm + Send + Sync + 'static`. The generic bound does not change - only the `execute` signature and body wrapping.
- `ConfiguredTool::execute` calls `tokio::process::Command` and `.await`s inside the body. The `async move` block captures `self` (via `&'a self`), which is safe because `ConfiguredTool: Send + Sync`.
- The `input: serde_json::Value` parameter is owned (moved into the `async move` block), so it carries no lifetime concern.
- Callers in `ToolExecutor::execute` call `tool.execute(...).await` - this works unchanged because `Pin<Box<dyn Future>>` implements `Future`.

**Validation:** `otto check` + `otto test`

### Phase 3: Remove crate and update rules

**3a: Remove crate dependency**

```bash
cargo remove async-trait
otto check
otto test
```

After Phases 1-2, zero source files import `async_trait`. The crate removal is clean.

**3b: Update `CLAUDE.md`**

Add to the "Conventions" section:

```
- No `async-trait` crate - use native `async fn` / `impl Future` for non-dyn traits;
  use manual `Pin<Box<dyn Future>>` for traits requiring dyn dispatch (e.g. Tool)
```

**3c: Update `rust.md`**

Add to the "Core Dependencies" section and/or a new "Banned Crates" section:

```
## Banned Crates

| Crate | Reason | Alternative |
|-------|--------|-------------|
| `async-trait` | Unnecessary since Edition 2024 / Rust 1.75+. Generates hidden `Pin<Box<dyn Future>>` that we can write explicitly when needed. | Native `async fn` in traits (non-dyn); manual `Pin<Box<dyn Future + Send + 'a>>` (dyn-required) |
```

**3d: Mark `tool-dyn-permanent.md` as superseded**

Update the status field:
```
**Status:** Superseded by 2026-04-05-async-trait-eradication.md
```

The decision that `Box<dyn Tool>` is correct stands. The decision that `#[async_trait]` is required for it does not.

### Phase 4: Remove dead domain modules

`domain::decision` and `domain::proposal` are registered in `Stores` with accessor methods and TaskStore indexing, but no production code path creates, queries, or mutates `Decision` or `Proposal` records. They are planned future infrastructure carrying `#[allow(dead_code)]` - a direct violation of the project's `#![deny(dead_code)]` stance.

**Files changed:**

| File | Change |
|------|--------|
| `src/domain.rs` | Remove `#[allow(dead_code)] pub mod decision;` and `#[allow(dead_code)] pub mod proposal;` |
| `src/domain/decision.rs` | Delete file |
| `src/domain/proposal.rs` | Delete file |
| `src/daemon/context.rs` | Remove `use crate::domain::decision::Decision`, `use crate::domain::proposal::Proposal`, the two `StdRwLock<HashMap<...>>` fields from `Stores`, their `store_accessors!` entries, TaskStore index rebuilding, and hydration logic |

Both modules are recoverable from git history (`git log --all -- src/domain/decision.rs`) when the feature work that needs them arrives.

**Validation:** `otto check` + `otto test`

## Alternatives Considered

### Alternative 1: Keep async_trait on Tool permanently

- **Description:** The approach from `tool-dyn-permanent.md` - accept async_trait as permanent for dyn-dispatched traits
- **Pros:** Zero effort, zero risk
- **Cons:** Keeps a proc-macro dependency that does nothing we cannot write in one line; prevents declaring async_trait as banned; leaves the impression the codebase couldn't complete the migration
- **Why not chosen:** The manual desugaring is trivial and the user explicitly wants the crate eradicated

### Alternative 2: Type-erased wrapper pattern for Tool

- **Description:** Keep `Tool` trait with native `async fn execute`, create a `DynTool` wrapper struct with an `ErasedTool` helper trait that boxes the future, store `HashMap<String, DynTool>` in `ToolExecutor`
- **Pros:** Tool impls stay clean with `async fn execute`; boxing happens in one place
- **Cons:** Two traits instead of one; extra indirection; more code; harder to understand
- **Why not chosen:** More complex for no practical benefit. The manual `Pin<Box<>>` on the trait itself is simpler and more honest about the cost

### Alternative 3: Enum dispatch for tools

- **Description:** Replace `Box<dyn Tool>` with `enum AnyTool { Read(ReadTool), Write(WriteTool), ... }`
- **Pros:** No boxing, no dyn dispatch, best performance
- **Cons:** Closed set - every new tool requires a variant; `ConfiguredTool` is runtime-created from user config, making it impossible to enumerate at compile time; massive match arms
- **Why not chosen:** Fundamentally incompatible with runtime tool registration

### Alternative 4: Keep decision/proposal modules, remove the allow

- **Description:** Remove `#[allow(dead_code)]` and wire the modules into production code
- **Pros:** Keeps planned infrastructure ready
- **Cons:** Wiring unused types into production paths is scope creep; the types have no consumers
- **Why not chosen:** Premature infrastructure. Recoverable from git when needed.

## Technical Considerations

### Dependencies

- `async-trait` removed (Phase 3)
- No new dependencies added
- `std::future::Future` and `std::pin::Pin` added to imports where needed (stdlib, zero cost)

### Performance

- Agent: no change (was already monomorphized via match dispatch)
- Tool: identical runtime cost - `#[async_trait]` generates the same `Pin<Box<dyn Future>>` we now write by hand
- Decision/Proposal removal: removes two unnecessary `StdRwLock<HashMap<>>` from `Stores`

### Testing Strategy

- `otto check` after each phase (compile + clippy + fmt)
- `otto test` after Phases 2, 3, and 4
- Current passing count: 2,339 tests - should remain unchanged through Phases 1-3
- Phase 4 may reduce count slightly (decision/proposal unit tests removed)

### Rollout Plan

```
refactor(agents): remove async_trait from Agent trait - native impl Future
refactor(tools): remove async_trait from Tool trait - manual Pin<Box<Future>>
chore: remove async-trait crate dependency
docs: ban async-trait in CLAUDE.md and rust.md; supersede tool-dyn-permanent
refactor(domain): remove orphaned decision and proposal modules
```

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Tool `execute` lifetime annotations wrong | Medium | Low | Compiler catches immediately; the `'a` pattern is proven in LlmClient/AgenticLlm |
| DelegateTool generic bounds interact poorly with Pin<Box<>> | Low | Medium | DelegateTool already works with dyn dispatch; the change is signature-only |
| Removing decision/proposal breaks a wiring path we missed | Low | Medium | Grep confirmed: only daemon/context.rs imports them; no IPC handler or agent references them |
| Future contributor adds async-trait back | Low | Low | Banned crate list in rust.md + CLAUDE.md catches it at review time |

## Audit Findings (informational)

The architectural review and Gemini Architect audit (both 2026-04-05) confirmed:

- **Build:** `cargo check` clean, `cargo clippy -D warnings` zero warnings, 2,339 tests pass, 0 fail, 2 ignored (API key gated)
- **ureq:** Fully removed from Cargo.toml and all source
- **Commented-out tests:** Zero - all 30 files from the parking-lot commits have been restored
- **`Arc<dyn>`:** Zero instances remain
- **`Box<dyn>`:** 3 instances remain, all `Box<dyn Tool>` in the tool registry (correct, addressed by Phase 2)
- **`.unwrap()` in production:** Zero - all 138 unwrap calls are confined to test code
- **`todo!()` / `unimplemented!()`:** Zero
- **`#[allow(unused...)]`:** Zero (outside test modules)

The codebase is healthy. This doc addresses the last two systematic violations: async_trait presence and dead_code allows.

## Open Questions

- [x] Is Agent ever used as `dyn Agent`? No - confirmed via grep. Monomorphic match dispatch only.
- [x] Can Tool's async method be made object-safe without async-trait? Yes - manual `Pin<Box<dyn Future>>`.
- [x] Are decision/proposal used in any production path? No - only in `Stores` registration and their own unit tests.

## References

- `docs/design/2026-04-05-post-async-migration-cleanup.md` - parent migration doc (Phases 1-3 implemented, Phase 4 complete)
- `docs/design/2026-04-05-tool-dyn-permanent.md` - superseded decision doc
- `docs/design/2026-04-05-comment-out-async-tests.md` - historical: test commenting strategy (all tests now restored)
- Rust RFC 3498 - async fn in traits (stable since Rust 1.75)
- `src/agents/executor/lifecycle.rs:320-371` - monomorphic Agent dispatch via match
- `src/tools/executor.rs:18` - `HashMap<String, Box<dyn Tool>>` heterogeneous storage
- Gemini Architect audit (2026-04-05) - independent verification of migration completeness
