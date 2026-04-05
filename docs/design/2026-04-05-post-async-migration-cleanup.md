# Design Document: Post-Async Migration Cleanup

**Author:** Scott A. Idler
**Date:** 2026-04-05
**Status:** Implemented (Phase 2 blocked - see body)
**Review Passes Completed:** 5/5

## Summary

The async migration that made `dispatch()` async left four cleanup items: 30 files with
commented-out tests awaiting `#[tokio::test]` conversion, a dead `ureq` dependency, ~35
`#[async_trait]` annotations that Edition 2024 no longer needs, and `decomposer.rs` passing
`&(dyn HttpClient + Sync)` through 8+ functions instead of a generic. This doc defines the
mechanical cleanup passes to retire all four items and leave the codebase in a clean async-native
state.

## Problem Statement

### Background

Making `dispatch()` async cascaded into every handler and every test that calls it. Two
checkpoint commits commented out 30 files worth of tests to keep the build green:

- `ec39075` - commented out 14 integration files + 2 inline test blocks + `ipc/client.rs` +
  `tui/run/ipc.rs` (first wave)
- `5080984` - commented out 16 agent module files (3 standalone + 13 inline) (second wave)

Those comments are a parking lot, not a permanent state. The same migration also surfaced three
structural leftovers: the `ureq` crate in `Cargo.toml` with zero source uses, `#[async_trait]`
annotations that predated Rust 1.75 native support, and a function-signature pattern in
`decomposer.rs` that violates `rust.md`.

### Problem

- **30 files with commented-out tests** block visibility into regressions. The build is green
  but coverage is dark across agents, integration, ipc, and tui layers.
- **Dead `ureq` dependency** adds compile-time surface with no benefit.
- **~35 `#[async_trait]` annotations** are redundant on Edition 2024. They generate
  `Pin<Box<dyn Future>>` internally and prevent native async-fn-in-trait optimizations.
- **`decomposer.rs` dyn pattern** passes `&(dyn HttpClient + Sync)` through 8+ function
  signatures, violating `rust.md`'s rule against `dyn` where generics are cleaner.

### Goals

- All commented-out tests restored and passing with `#[tokio::test]`
- `ureq` removed from `Cargo.toml` and `Cargo.lock`
- Zero `#[async_trait]` annotations in production code
- `decomposer.rs` uses `<H: HttpClient + Sync>` generic parameter
- `otto check` and `otto test` pass after each phase

### Non-Goals

- Converting `Box<dyn LlmClient>` in agents to generics (high blast radius, deferred)
- Converting `Box<dyn Tool>` in `ToolExecutor` (heterogeneous collection - correct as-is)
- Converting `Box<dyn HttpClient>` in `ValidatorClient` struct field (test-injection pattern -
  acceptable)
- Any production behavior changes

## Proposed Solution

### Overview

Four independent cleanup phases in dependency order. Each phase ends with `otto check` +
`otto test` green before the next begins.

```
Phase 1: Remove ureq             (1 cargo remove, 1 commit)
Phase 2: Drop async_trait        (~35 annotation deletions + 1 cargo remove, 1 commit)
Phase 3: Generify decomposer     (8 function signature changes, 1 commit)
Phase 4: Restore commented tests (30 files, file-by-file, 30 commits)
```

### Phase 1: Remove ureq

`ureq` appears in `Cargo.toml` at version `3.2.0` with zero `use ureq` or `ureq::` occurrences
anywhere in `src/`. It was a prior sync-HTTP client that got replaced by `reqwest` during the
validator migration.

```bash
cargo remove ureq
otto check
```

### Phase 2: Drop async_trait (BLOCKED - prerequisite missing)

**Status: Blocked.** Attempted during execution; discovered that all four async traits in this
codebase are used as `dyn Trait` object types, making them dyn-incompatible once `#[async_trait]`
is removed. Native `async fn` in traits (RFC 3498) are NOT object-safe - each implementation
returns its own opaque future type, breaking vtable dispatch.

**Blocked by `dyn` usages:**

| Trait | dyn usage | Location |
|-------|-----------|----------|
| `LlmClient` | `Box<dyn LlmClient>` | `coordinator.rs`, `implementer.rs`, `reviewer.rs` |
| `Tool` | `Box<dyn Tool>`, `HashMap<String, Box<dyn Tool>>` | `executor.rs`, `builtin.rs` |
| `HttpClient` | `Box<dyn HttpClient>` | `validator/client.rs` |
| `AgenticLlm` | `&dyn AgenticLlm`, `Arc<dyn AgenticLlm>` | `agentic_loop.rs`, `delegate.rs` |

**Path to unblock:** Convert all four to generics first:
- `LlmClient` → `<L: LlmClient>` on agent structs
- `Tool` → refactor `ToolExecutor` to not require `dyn Tool` (or use enum dispatch)
- `HttpClient` → `<H: HttpClient>` on `ValidatorClient` struct
- `AgenticLlm` → `<L: AgenticLlm>` on `ToolExecutor` and `DelegateTool`

Each conversion is its own design doc and refactor. Phase 2 is deferred until all four are done.

**Scope - live production code only (test-only usages are inside `/* */` already):**

| File | Count | Context |
|------|-------|---------|
| `src/tools/traits.rs` | 1 | `Tool` trait definition |
| `src/tools/builtin/*.rs` | ~14 | One per builtin tool `impl Tool` |
| `src/tools/configured.rs` | 1 | `ConfiguredTool` impl |
| `src/tools/agentic_loop.rs` | 3 | `ToolDispatch` trait + impls |
| `src/agents.rs` | 1 | `LlmClient` trait definition |
| `src/agents/llm_client.rs` | 2 | `LlmClient` impls |
| `src/agents/coordinator.rs` | 1 | `CoordinatorAgent` impl |
| `src/agents/implementer.rs` | 2 | `ImplementerAgent` trait + impl |
| `src/agents/integrator.rs` | 1 | `IntegratorAgent` impl |
| `src/agents/researcher.rs` | 1 | `ResearcherAgent` impl |
| `src/agents/reviewer.rs` | 1 | `ReviewerAgent` impl |
| `src/validator/client.rs` | 2 | `HttpClient` trait + `ReqwestClient` impl |

**Procedure:**
1. For each file: delete `use async_trait::async_trait;` and every `#[async_trait]` attribute
2. `cargo remove async-trait`
3. `otto check`

**Note:** `#[async_trait]` usages inside `/* */` blocks are inert. They will be cleaned up
file-by-file during Phase 4 as each file is restored.

### Phase 3: Convert decomposer.rs to generic HttpClient

`decomposer.rs` passes `&(dyn HttpClient + Sync)` through 8 internal functions. This violates
`rust.md` ("use generics for DI, never `dyn` trait objects or `Box<dyn ...>`").

**Change pattern (8 occurrences at lines 177, 234, 285, 296, 354, 467, 484, 520):**

```rust
// Before
fn some_fn(http_client: &(dyn HttpClient + Sync), ...) -> Result<...> { ... }

// After
fn some_fn<H: HttpClient + Sync>(http_client: &H, ...) -> Result<...> { ... }
```

All external call sites pass concrete `&ReqwestClient` or a test mock (`SequenceMockHttp` which
already implements `HttpClient`). No call-site changes are needed beyond threading the type
parameter through the top-level public function, which the compiler will guide.

```bash
otto check   # compiler points to every remaining dyn site
otto test
```

### Phase 4: Restore Commented-Out Tests

All 30 files have the same root cause: sync `#[test]` calling async code. The fix is mechanical:
`#[test]` + `fn` becomes `#[tokio::test]` + `async fn`, and every async call gets `.await`.

**Critical: bridge tests require multi-thread runtime.**
`AgentIpcBridge` uses `block_in_place` internally. Any test that constructs a bridge must use:
```rust
#[tokio::test(flavor = "multi_thread")]
async fn test_foo() { ... }
```
This applies to all tests in `src/agents/bridge.rs` and any other file that calls
`AgentIpcBridge::new(...)` directly.

**Batch A - integration tests (from ec39075, 12 files):**

| File | Shape |
|------|-------|
| `src/tests/integration/learning.rs` | per-fn `/* */` |
| `src/tests/integration/pool.rs` | per-fn `/* */` |
| `src/tests/integration/coordinator.rs` | per-fn `/* */` |
| `src/tests/integration/cycling.rs` | per-fn `/* */` |
| `src/tests/integration/executor.rs` | per-fn `/* */` |
| `src/tests/integration/fsm.rs` | per-fn `/* */` |
| `src/tests/integration/locks.rs` | per-fn `/* */` |
| `src/tests/integration/preformed.rs` | per-fn `/* */` |
| `src/tests/integration/hierarchy.rs` | per-fn `/* */` |
| `src/tests/integration/pipeline.rs` | per-fn `/* */` |
| `src/tests/integration/sessions.rs` | per-fn `/* */` |
| `src/tests/integration/tick.rs` | per-fn `/* */` |

**Batch B - ipc/tui (from ec39075, 2 files):**

| File | Shape |
|------|-------|
| `src/ipc/client.rs` | per-fn `/* */` |
| `src/tui/run/ipc.rs` | per-fn `/* */` |

**Batch C - agent inline mods (from 5080984, 13 files):**
Work smallest-first to get fast feedback:

| File | Shape | Bridge? |
|------|-------|---------|
| `src/agents/executor/action/learning.rs` | inline mod | no |
| `src/agents/executor/action/tool.rs` | inline mod | no |
| `src/agents/executor/action/validation.rs` | inline mod | no |
| `src/agents/executor/action/lock.rs` | inline mod | no |
| `src/agents/executor/action/record.rs` | inline mod | no |
| `src/agents/executor/util.rs` | inline mod | no |
| `src/agents/executor/lifecycle.rs` | inline mod | no |
| `src/agents/coordinator/run.rs` | inline mod | no |
| `src/agents/executor/action/file.rs` | inline mod | no |
| `src/agents/executor/action/bundle.rs` | inline mod | no |
| `src/agents/executor/action/work.rs` | inline mod | no |
| `src/agents/reviewer.rs` | inline mod | no |
| `src/agents/bridge.rs` | inline mod | **yes** - multi_thread |

**Batch D - agent standalone test files (from 5080984, 3 files):**
These files are wrapped in a single `/* */` block. After unwrapping:
1. Remove `/*` first line and `*/` last line
2. Convert all `#[test]` to `#[tokio::test]`
3. Add `.await` to all async calls
4. Remove `#![allow(unused_imports, dead_code)]` header (no longer needed once live)
5. Add `#[tokio::test]` to any mock impl test helpers as needed

| File | Fns | Bridge? |
|------|-----|---------|
| `src/agents/integrator/tests.rs` | 57 | no |
| `src/agents/implementer/tests.rs` | 80 | no |
| `src/agents/coordinator/tests.rs` | 92 | no |

**Process per file:**
```bash
# 1. Unwrap/restore the file
# 2. Convert annotations and add .await
otto check    # verify it compiles
otto test     # verify all tests pass
# 3. Commit: refactor(tests): restore N async tests in <module>
```

## Alternatives Considered

### Alternative 1: Keep async_trait indefinitely

- **Description:** Leave all `#[async_trait]` annotations as-is
- **Pros:** Zero effort, zero risk
- **Cons:** Dead dependency; prevents native RPITIT optimizations; inconsistent with Edition 2024
- **Why not chosen:** Pure overhead with no benefit on Edition 2024

### Alternative 2: Convert all 30 test files in one commit

- **Description:** Single large PR restoring all commented tests at once
- **Pros:** One commit, faster on paper
- **Cons:** Impossible to bisect; one bridge test without `multi_thread` poisons the whole batch;
  any unanticipated async interaction blocks all 30 files
- **Why not chosen:** File-by-file with `otto test` gates is the proven pattern from ec39075

### Alternative 3: Keep dyn in decomposer.rs

- **Description:** Leave `&(dyn HttpClient + Sync)` as-is
- **Pros:** No change needed
- **Cons:** Violates `rust.md`; prevents monomorphization; 8 signatures carry unnecessary
  object-safety constraints
- **Why not chosen:** Generic conversion is mechanical and verified by the compiler

### Alternative 4: Convert Box<dyn LlmClient> to generics now

- **Description:** Add `<L: LlmClient>` type parameter to `Coordinator`, `Implementer`,
  `Reviewer`, `Researcher` structs
- **Pros:** Full `rust.md` compliance
- **Cons:** Touches every agent constructor, daemon supervisor, and all test fixtures; not
  mechanical; high blast radius for a cleanup pass
- **Why not chosen:** Deferred to a dedicated refactor doc; not a blocker

## Technical Considerations

### Dependencies

- `ureq` removed (Phase 1)
- `async-trait` removed (Phase 2)
- No new dependencies

### Testing Strategy

- `otto check` after Phase 1 and Phase 2 (no test changes)
- `otto check` + `otto test` after Phase 3
- `otto check` + `otto test` after each file in Phase 4 (30 validation gates)
- Final `otto ci` when all phases complete

### Rollout Plan

```
chore(cleanup): remove ureq dead dependency
chore(cleanup): drop async_trait - use native async fn in traits (Edition 2024)
refactor(decomposer): convert dyn HttpClient to generic H: HttpClient + Sync
refactor(tests): restore N async tests in <module>  (x30 commits)
```

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Bridge tests panic without multi-thread runtime | High | Low | Use `flavor = "multi_thread"` on all bridge-touching tests |
| async_trait removal breaks a dyn-dispatch call site | Low | High | `otto check` catches it immediately; single annotation restores it |
| async_trait removal inside `/* */` breaks compilation when `async-trait` crate removed | Low | Med | Verify: cargo remove after phase 2, before phase 4; commented code must not reference removed crate in type position |
| Test restoration reveals new async bugs masked by comments | Medium | Medium | Expected and desirable; fix forward per bug |
| decomposer generic propagates to unexpected callers | Low | Low | Compiler guides every missed site; all callers currently use concrete types |
| Integration tests need spawn/runtime context beyond `#[tokio::test]` | Low | Med | Check each integration test for `spawn_blocking` or `block_in_place` usage |

## Open Questions

- [ ] Do any integration tests (`src/tests/integration/`) use `AgentIpcBridge` directly and
  therefore need `flavor = "multi_thread"`?
- [ ] Are there `#[async_trait]` usages within `/* */` blocks that reference the `async_trait`
  crate via `async_trait::async_trait` path (not the use-import form)? If so, removing the crate
  in Phase 2 could cause those inert commented blocks to fail to parse. Check with
  `grep -r 'async_trait::async_trait' src/` before cargo remove.

## References

- Commit `ec39075` - first wave of test commenting
- Commit `5080984` - second wave (16 agent module files)
- `docs/design/2026-04-05-comment-out-async-tests.md` - parking-lot strategy doc
- Rust RFC 3498 - async fn in traits (stable Rust 1.75)
- `src/agents/bridge.rs` - `AgentIpcBridge` with `block_in_place` (requires multi-thread runtime)
- `src/decomposer.rs` - 8 `dyn HttpClient + Sync` function signatures
