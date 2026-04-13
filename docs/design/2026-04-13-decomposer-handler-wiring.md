# Design Document: Decomposer IPC Handler Wiring

**Author:** Scott A. Idler
**Date:** 2026-04-13
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Wire the `decomposer.decompose` IPC handler and remaining decomposer handlers (`ratify`, `re_decompose`, `abandon_children`) into the daemon dispatch table, using `TaskStore::create_many` for atomic batch child persistence. Switch the `doc.accept` entry path from the monolithic `decompose_hierarchy` background task to the engine-driven decomposition flow established in Doc 6. Delete `src/decomposer.rs` once no live callers remain.

## Problem Statement

### Background

Doc 6 (2026-04-11-decomposer-as-strategy) established the engine-driven decomposition architecture: triggers, strategies, role configs, and a thin DecomposerAgent. Phases 1-4 are committed. The DecomposerAgent calls `bridge.request("decomposer.decompose", params)`, but no handler exists for that IPC method. The agent correctly surfaces this as `decomposition.failed`.

The legacy `decompose_hierarchy` function in `src/decomposer.rs` (2,177 lines, ~970 impl + ~1,200 tests) is still the live decomposition path, called from `src/daemon/handlers/doc.rs:241` via a background tokio task. The `Decompose` primitive in `src/primitive/catalog/decompose.rs` is an IPC stub that bridges to the same nonexistent `decomposer.decompose` handler.

`TaskStore::create_many` shipped in `scottidler/taskstore@v0.2.3` (already locked in Cargo.lock), removing the last external dependency blocker.

### Problem

The engine-driven decomposition path is fully wired but terminally non-functional: every decomposer agent invocation fails because the IPC handler doesn't exist. The legacy path works but bypasses the engine, defeating the goals of Doc 6 (YAML-composable decomposition, crash resilience, configurable depth).

### Goals

- Wire `decomposer.decompose` handler that performs single-level decomposition with atomic child persistence via `create_many`
- Wire `decomposer.ratify`, `decomposer.re_decompose`, `decomposer.abandon_children` handlers
- Switch `doc.accept`/`doc.inject` from the background `decompose_hierarchy` task to engine-driven decomposition
- Enable the 3 ignored E2E decomposition tests and verify they pass
- Delete `src/decomposer.rs` once all live callers are removed

### Non-Goals

- Changing what the LLM call produces (prompt templates, tool schemas, temperature)
- Modifying the DecomposerAgent logic (already complete in Doc 6)
- Changing the engine trigger/strategy/role-config YAML (already complete in Doc 6)
- Adding new domain types or FSM states
- Streaming decomposition progress to TUI

## Proposed Solution

### Overview

Extract the single-level decomposition core from `src/decomposer.rs` into `src/daemon/handlers/decomposer.rs` as IPC handlers. The key function is `decompose_into` (~137 lines) which performs one LLM call (via `call_llm_for_children`), parses children, validates, detects cycles, and resolves dependencies. The multi-level orchestrator `decompose_hierarchy` is NOT extracted - multi-level flow emerges from engine ticks per Doc 6.

### What Moves Where

| Source (decomposer.rs) | Destination | Reason |
|---|---|---|
| `decompose_into` | handlers/decomposer.rs `handle_decomposer_decompose` | Core single-level logic, now an IPC handler |
| `call_llm_for_children`, `call_llm_for_children_raw` | handlers/decomposer.rs (private helpers) | LLM call + structured response parsing |
| `build_decompose_prompt` | handlers/decomposer.rs (private helper) | Prompt construction for LLM call |
| `decomposition_tool_schema` | handlers/decomposer.rs (private helper) | Tool-use schema for structured output |
| `detect_cycles` | handlers/decomposer.rs (private helper) | DAG validation |
| `extract_acceptance_criteria` | handlers/decomposer.rs (private helper) | Markdown parsing |
| `call_llm_for_ratification`, `build_ratify_prompt` | handlers/decomposer.rs (private helpers) | Ratification LLM call |
| `call_llm_for_validation`, `build_validate_prompt` | handlers/decomposer.rs (private helpers) | Validation LLM call |
| `ratify_hierarchy` | handlers/decomposer.rs `handle_decomposer_ratify` | Bottom-up validation handler |
| `ChildRecord`, `ChildEntry`, `ValidationResult`, `RatifyResult` | handlers/decomposer.rs (private types) | Internal decomposition types |
| `expected_dep_prefix` | handlers/decomposer.rs (private helper) | ID prefix for dependency resolution |
| `decompose_hierarchy` | DELETED | Multi-level orchestration replaced by engine |
| `decompose_spec_branch`, `decompose_phase_branch` | DELETED | Branch orchestration replaced by engine |
| `records_to_hierarchy`, `DecomposedHierarchy` | DELETED | Handler creates domain records directly |
| `extract_title_from_markdown` | DELETED | Only used by records_to_hierarchy |
| `persist_hierarchy` (doc.rs) | DELETED | Handler persists via create_many |

### Handler: `decomposer.decompose`

**Input params:**
```json
{
  "parent_id": "sp-abc123",
  "parent_collection": "spec",
  "target_kind": "phase",
  "count_guidance": "1-5",
  "dependency_pattern": "sequential-chain"
}
```

**Logic:**
1. Read parent record from stores
2. Build prompt from parent content + target kind template
3. Call LLM via HttpClient (Anthropic Messages API with tool-use)
4. Parse structured response into `Vec<ChildEntry>`
5. Generate domain IDs for each child
6. Detect dependency cycles (`detect_cycles`)
7. Resolve dependency titles to sibling IDs (local resolution only)
8. Build domain records (Spec, Phase, or Work depending on target_kind)
9. Persist all children atomically via `TaskStore::create_many`
10. Insert into in-memory stores
11. Emit `record_created` events per child
12. Return child count and IDs

**Atomic persistence (step 9):** `create_many` writes all JSONL records in a single buffer. If the process crashes mid-call, either all children land or none do. This is the atomicity guarantee from the Doc 6 Architect review.

**Result:**
```json
{
  "children": [
    { "id": "ph-xxx", "title": "..." },
    { "id": "ph-yyy", "title": "..." }
  ],
  "child_count": 2
}
```

### Handler: `decomposer.ratify`

Calls the existing `ratify_hierarchy` logic: groups children by parent, validates each parent-children set via LLM. Returns `{ "passed": bool, "issues": [...] }`.

### Handler: `decomposer.re_decompose`

Abandons non-preserved children, increments `decomposition_attempts` on the parent, then calls `decomposer.decompose` with the same params. Used by the `re-decompose-on-gaps` strategy.

### Handler: `decomposer.abandon_children`

Transitions all non-terminal children of a parent to Abandoned, preserving specified IDs. Used by re-decomposition to clear gaps before regenerating.

### Entry Path Switch: `doc.accept`

**Current flow (v3):**
```
doc.accept -> accept_plan_markdown -> spawn(decompose_hierarchy) -> persist_hierarchy
```

**New flow (v4):**
```
doc.accept -> accept_plan_markdown -> create Plan (Active) -> engine tick -> plan-decomposable fires -> spawn-agent(decomposer) -> decomposer.decompose handler
```

Changes to `accept_plan_markdown`:
1. Create Plan record directly from markdown (title, AC, content) via `plan.create` IPC
2. Classify tier (brief/full) and set on plan record
3. Transition Plan to Active via `plan.transition` IPC
4. Remove the `decompose_hierarchy` background task spawn (lines 224-280 of doc.rs)
5. The `Decomposing` coordinator state is KEPT - the coordinator starts in `Decomposing` as before. The engine's `decomposition.completed` event (emitted by the DecomposerAgent) transitions the coordinator state from `Decomposing` to `Planning`. This prevents the Coordinator from spin-looping on a false "all planning artifacts have been decomposed" premise while the engine is still creating children.
6. Start Coordinator agent as before
7. The engine's `plan-decomposable` trigger fires on the next tick (plan is Active with no spec children)

`persist_hierarchy` in doc.rs (lines 365-415) is deleted - the handler writes records directly via `create_many`.

**Note on HttpClient:** The current `decompose_into` is generic over `HttpClient`. The handler instantiates `ReqwestClient` directly from config (same as the current doc.rs background task does at line 240). The generic trait boundary is not needed in the handler.

### Implementation Plan

#### Phase 1: Wire `decomposer.decompose` handler
**Model:** opus

1. Create `src/daemon/handlers/decomposer.rs` with `handle_decomposer_decompose` (async handler using `try_async_handler!` macro)
2. Extract `decompose_into`, `call_llm_for_children`, `call_llm_for_children_raw`, `build_decompose_prompt`, `decomposition_tool_schema`, `detect_cycles`, `extract_acceptance_criteria`, `expected_dep_prefix` from `src/decomposer.rs`
3. Parameterize `build_decompose_prompt` to inject `count_guidance` and `dependency_pattern` into the LLM prompt (currently hardcoded in `.pmt` templates). Without this, the role config YAML values are ignored.
4. Replace generic `HttpClient` trait bound with concrete `ReqwestClient` instantiation from config
5. Persist children atomically via `TaskStore::create_many`; insert into in-memory stores; write `docs/loopr/<id>.md` files; emit `record_created` events
6. Add `"decomposer.decompose"` to dispatch table in `src/daemon/handlers.rs`
7. Unit tests: mock LLM, verify child creation, cycle detection, dependency resolution, create_many usage

#### Phase 2: Switch entry path
**Model:** opus

1. Modify `accept_plan_markdown` in doc.rs: create Plan record, transition to Active, remove background `decompose_hierarchy` task
2. Keep `Decomposing` coordinator state - the engine's `decomposition.completed` event transitions to `Planning`
3. Remove `persist_hierarchy` function from doc.rs
4. Remove `use crate::decomposer::*` imports from doc.rs
5. Verify coordinator agent starts correctly with the decomposition gate still intact
6. Unit tests: coordinator state transition on decomposition.completed

#### Phase 3: Wire remaining handlers and failure coordination
**Model:** sonnet

1. Add `handle_decomposer_ratify` (extract `ratify_hierarchy`, `call_llm_for_ratification`, `build_ratify_prompt`)
2. Add `handle_decomposer_abandon_children` (transition children to Abandoned, preserve IDs)
3. Add `handle_decomposer_re_decompose` (abandon + increment `decomposition_attempts` + re-invoke decompose)
4. Add all three to dispatch table
5. Wire `decomposition.failed` event -> `CoordinatorState.decomposition_error` write (Architect finding: without this, the coordinator hangs in Decomposing with no error context on agent failure)
6. Ensure DecomposerAgent LLM failures result in a terminal agent session (Failed), not a silent retry loop. The `no-active-sessions` guard prevents re-spawn while the session is active; when it terminates as Failed, `plan-decomposable` would re-fire. The `decomposition-attempt-limit` trigger (already in reconciliation.yml) must be wired to a strategy that transitions the plan to Abandoned on repeated failures.
7. Unit tests for each handler + failure coordination path

#### Phase 4: Enable E2E tests and delete legacy
**Model:** sonnet

1. Remove `#[ignore]` from 3 E2E decomposition tests in `src/tests/integration/decomposition.rs`
2. Run tests, fix any assertion failures
3. Verify `decompose_hierarchy` has zero live callers (`grep -rn decompose_hierarchy src/ --include="*.rs" | grep -v decomposer.rs | grep -v test`)
4. Delete `src/decomposer.rs`; remove `pub mod decomposer;` from `src/lib.rs`
5. Run `otto ci` - verify no dead code, all tests pass

## Alternatives Considered

### Alternative 1: Keep decomposer.rs as a library, call from handler

- **Description:** Don't extract functions. Have the handler call `decomposer::decompose_into` directly.
- **Pros:** Minimal code movement. No risk of extraction bugs.
- **Cons:** decomposer.rs stays as a 2,200-line monolith. The handler becomes a thin wrapper that doesn't own its logic. The multi-level orchestration code (`decompose_hierarchy`) remains in the codebase unused.
- **Why not chosen:** The goal is to delete decomposer.rs. Keeping it as a library defeats the purpose. The extraction is straightforward since `decompose_into` has a clean function boundary.

### Alternative 2: Inline everything into the DecomposerAgent

- **Description:** Move the LLM call logic into the agent's `run()` method instead of an IPC handler.
- **Pros:** Fewer IPC hops. Agent owns its full lifecycle.
- **Cons:** The `Decompose` primitive also needs to call this logic (it bridges to `decomposer.decompose`). If the logic lives in the agent, the primitive can't reach it. The handler pattern keeps the logic accessible to both the agent and the primitive.
- **Why not chosen:** Handler pattern is consistent with how all other domain operations work in Loopr.

## Technical Considerations

### Dependencies

- **Internal:** `TaskStore::create_many` (v0.2.3, already locked), `HttpClient` trait (validator/client.rs), domain record constructors (Plan, Spec, Phase, Work), DecomposerAgent (Doc 6)
- **External:** None new

### Performance

- Single-level decomposition is one LLM call (~5-15s). No change from v3 per-level latency.
- `create_many` writes all children in one syscall. Faster than sequential `create` calls.
- Entry path switch removes the background task overhead (one fewer tokio spawn per plan).

### Security

- No new external inputs. Handler params come from the engine/agent, not user IPC.
- LLM response parsing is unchanged from v3.

### Testing Strategy

- Phase 1: Unit tests with mocked HttpClient verifying child creation, cycle detection, atomic persistence
- Phase 2: Unit tests for ratify/abandon/re-decompose handlers
- Phase 3: Integration tests verifying doc.accept creates Active plan and engine takes over
- Phase 4: Existing E2E tests (currently ignored) enabled and passing

### Rollout Plan

- All phases on v4 branch
- Phase 4 is the gate before deleting decomposer.rs
- Each phase committed separately with otto ci green

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Extraction introduces subtle behavioral differences from v3 | Medium | Medium | E2E tests in Phase 4 verify structural equivalence |
| doc.accept entry path change breaks coordinator startup | Low | High | Phase 3 tests verify coordinator starts correctly |
| create_many has different semantics than sequential creates | Low | Low | create_many is already tested upstream; Phase 1 unit tests verify behavior |
| Removing Decomposing state breaks coordinator FSM | Medium | Medium | Phase 3 checks all coordinator state transitions still work |
| 1,200 lines of decomposer.rs tests become orphaned | Low | Low | Tests that verify single-level decomposition are adapted to test the handler; multi-level tests are deleted (engine ticks cover that flow) |

## Resolved Questions

- **Handler is async (LLM call) - does the dispatch table support this?** Yes. `try_async_handler!` macro exists (handlers.rs:63) and the dispatch function is already `async fn`. Several existing handlers use it.
- **Should the handler do inline validation or leave it to the engine?** Leave it to the engine. The current `decompose_into` calls `call_llm_for_validation` inline, but Doc 6's `validate-after-decomposition` strategy handles this as an optional engine concern. The handler should NOT validate - it decomposes and persists only. The DecomposerAgent applies validation based on role config (Doc 6, step 8).
- **What about `docs/loopr/<id>.md` markdown files?** The handler writes them after persistence, same as `persist_hierarchy` does today. This is advisory (log-and-continue on failure). The LLM context builder reads from stores, not from disk files. Missing markdown files don't break the engine.
- **Should the `Decomposing` coordinator state be removed?** No. Keep it. The coordinator needs a gate to avoid spin-looping on "all planning artifacts have been decomposed" while the engine is still creating children. The engine's `decomposition.completed` event transitions the coordinator from `Decomposing` to `Planning`.
- **Should `CoordinatorFsmState::Decomposing` enum variant be deleted?** No. Existing TaskStore records may contain `Decomposing` state. Deleting the variant would cause serde deserialization panics on daemon restart.
- **count_guidance/dependency_pattern in prompts:** Phase 1 must parameterize `build_decompose_prompt` to inject these values. The current `.pmt` templates hardcode them. Without this fix, the YAML role config values are ignored.

## Open Questions

- [ ] Should `detect_cycles` move to a shared utility module (used by both handler and potential future validation), or stay private in the handler?

## References

- `docs/design/2026-04-11-decomposer-as-strategy.md` - Doc 6: engine-driven decomposition architecture
- `src/decomposer.rs` - Legacy monolith being replaced
- `src/daemon/handlers/doc.rs` - Current entry path with decompose_hierarchy call
- `src/agents/decomposer.rs` - DecomposerAgent (Doc 6 Phase 3)
- `src/primitive/catalog/decompose.rs` - Decompose primitive (IPC stub)
- `src/tests/integration/decomposition.rs` - E2E tests (currently ignored)
